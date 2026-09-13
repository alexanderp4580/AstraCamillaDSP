// CamillaDSP - A flexible tool for processing audio
// Copyright (C) 2026 Henrik Enquist
//
// This file is part of CamillaDSP.
//
// CamillaDSP is free software; you can redistribute it and/or modify it
// under the terms of either:
//
// a) the GNU General Public License version 3,
//    or
// b) the Mozilla Public License Version 2.0.
//
// You should have received copies of the GNU General Public License and the
// Mozilla Public License along with this program. If not, see
// <https://www.gnu.org/licenses/> and <https://www.mozilla.org/MPL/2.0/>.

use crate::ProcessingParameters;
use crate::SpectrumStatus;
use crate::audiodevice::*;
use crate::config;
use crate::pipeline;
use audio_thread_priority::{
    demote_current_thread_from_real_time, promote_current_thread_to_real_time,
};
use parking_lot::RwLock;
use std::sync::{Arc, Barrier};
use std::thread;

pub fn run_processing(
    conf_proc: config::Configuration,
    barrier_proc: Arc<Barrier>,
    tx_pb: crossbeam_channel::Sender<AudioMessage>,
    rx_cap: crossbeam_channel::Receiver<AudioMessage>,
    rx_pipeconf: crossbeam_channel::Receiver<(config::ConfigChange, config::Configuration)>,
    processing_params: Arc<ProcessingParameters>,
    spectrum_status: Arc<RwLock<SpectrumStatus>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let chunksize = conf_proc.devices.chunksize;
        let samplerate = conf_proc.devices.samplerate;
        // Spectrum FFT: planned once here (thread-local, no lock contention) and reused
        // every chunk. Only the small resulting `bins_db` snapshot crosses the lock in
        // `update_spectrum_status`. Chunksize is fixed for the life of this thread — a
        // device-type config change tears this thread down and a new one is spawned.
        let mut spectrum_planner = realfft::RealFftPlanner::<crate::PrcFmt>::new();
        let spectrum_fft = spectrum_planner.plan_fft_forward(chunksize);
        let spectrum_window: Vec<crate::PrcFmt> = (0..chunksize)
            .map(|i| {
                let x = 2.0 * std::f64::consts::PI * i as f64 / (chunksize as f64 - 1.0);
                (0.5 - 0.5 * x.cos()) as crate::PrcFmt
            })
            .collect();
        let mut spectrum_mono = spectrum_fft.make_input_vec();
        let mut spectrum_output = spectrum_fft.make_output_vec();
        let mut spectrum_scratch = spectrum_fft.make_scratch_vec();
        let multithreaded = conf_proc.devices.multithreaded();
        let nbr_threads = conf_proc.devices.worker_threads();
        let hw_threads = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or_default();
        if nbr_threads > hw_threads && multithreaded {
            warn!(
                "Requested {nbr_threads} worker threads. For optimal performance, this number should not \
                exceed the available CPU cores, which is {hw_threads}."
            );
        }
        if hw_threads == 1 && multithreaded {
            warn!(
                "This system only has one CPU core, multithreaded processing is not recommended."
            );
        }
        if nbr_threads == 1 && multithreaded {
            warn!(
                "Requested multithreaded processing with one worker thread. \
                   Performance can improve by adding more threads or disabling multithreading."
            );
        }
        let mut pipeline = pipeline::Pipeline::from_config(conf_proc, processing_params.clone());
        debug!("build filters, waiting to start processing loop");

        let thread_handle =
            match promote_current_thread_to_real_time(chunksize as u32, samplerate as u32) {
                Ok(h) => {
                    debug!("Processing thread has real-time priority.");
                    Some(h)
                }
                Err(err) => {
                    warn!("Processing thread could not get real time priority, error: {err}");
                    None
                }
            };

        // Initialize rayon thread pool
        if multithreaded {
            match rayon::ThreadPoolBuilder::new()
                .num_threads(nbr_threads)
                .build_global()
            {
                Ok(_) => {
                    debug!(
                        "Initialized global thread pool with {} workers",
                        rayon::current_num_threads()
                    );
                    rayon::broadcast(|_| {
                        match promote_current_thread_to_real_time(
                            chunksize as u32,
                            samplerate as u32,
                        ) {
                            Ok(_) => {
                                debug!(
                                    "Worker thread {} has real-time priority.",
                                    rayon::current_thread_index().unwrap_or_default()
                                );
                            }
                            Err(err) => {
                                warn!(
                                    "Worker thread {} could not get real time priority, error: {}",
                                    rayon::current_thread_index().unwrap_or_default(),
                                    err
                                );
                            }
                        };
                    });
                }
                Err(err) => {
                    warn!("Failed to build thread pool, error: {err}");
                }
            };
        }

        barrier_proc.wait();
        debug!("Processing loop starts now!");
        loop {
            match rx_cap.recv() {
                Ok(AudioMessage::Audio(mut chunk)) => {
                    //trace!("AudioMessage::Audio received");
                    chunk = pipeline.process_chunk(chunk);
                    crate::update_spectrum_status(
                        &spectrum_status,
                        spectrum_fft.as_ref(),
                        &spectrum_window,
                        &mut spectrum_mono,
                        &mut spectrum_output,
                        &mut spectrum_scratch,
                        &chunk.waveforms,
                        samplerate,
                    );
                    let msg = AudioMessage::Audio(chunk);
                    if tx_pb.send(msg).is_err() {
                        info!("Playback thread has already stopped.");
                        break;
                    }
                }
                Ok(AudioMessage::EndOfStream) => {
                    trace!("AudioMessage::EndOfStream received");
                    let msg = AudioMessage::EndOfStream;
                    if tx_pb.send(msg).is_err() {
                        info!("Playback thread has already stopped.");
                    }
                    break;
                }
                Ok(AudioMessage::Pause) => {
                    trace!("AudioMessage::Pause received");
                    let msg = AudioMessage::Pause;
                    if tx_pb.send(msg).is_err() {
                        info!("Playback thread has already stopped.");
                        break;
                    }
                }
                Err(err) => {
                    error!("Message channel error: {err}");
                    let msg = AudioMessage::EndOfStream;
                    if tx_pb.send(msg).is_err() {
                        info!("Playback thread has already stopped.");
                    }
                    break;
                }
            }
            if let Ok((diff, new_config)) = rx_pipeconf.try_recv() {
                trace!("Message received on config channel");
                match diff {
                    config::ConfigChange::Pipeline | config::ConfigChange::MixerParameters => {
                        debug!("Rebuilding pipeline.");
                        let new_pipeline =
                            pipeline::Pipeline::from_config(new_config, processing_params.clone());
                        pipeline = new_pipeline;
                    }
                    config::ConfigChange::FilterParameters {
                        filters,
                        mixers,
                        processors,
                    } => {
                        debug!("Updating parameters of filters: {filters:?}, mixers: {mixers:?}.");
                        pipeline.update_parameters(new_config, &filters, &mixers, &processors);
                    }
                    config::ConfigChange::Devices => {
                        let msg = AudioMessage::EndOfStream;
                        tx_pb.send(msg).unwrap();
                        break;
                    }
                    _ => {}
                };
            };
        }
        processing_params.set_processing_load(0.0);
        processing_params.set_resampler_load(0.0);
        if let Some(h) = thread_handle {
            match demote_current_thread_from_real_time(h) {
                Ok(_) => {
                    debug!("Processing thread returned to normal priority.")
                }
                Err(_) => {
                    warn!("Could not bring the processing thread back to normal priority.")
                }
            };
        }
    })
}
