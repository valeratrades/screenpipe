use crate::{DatabaseManager, VideoCapture};
use anyhow::Result;
use chrono::Utc;
use crossbeam::queue::SegQueue;
use log::{debug, error, info, warn};
use screenpipe_audio::{
    create_whisper_channel, record_and_transcribe, AudioDevice, AudioInput,
    AudioTranscriptionEngine, DeviceControl, TranscriptionResult,
};
use screenpipe_core::pii_removal::remove_pii;
use screenpipe_integrations::friend_wearable::initialize_friend_wearable_loop;
use screenpipe_vision::OcrEngine;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

pub async fn start_continuous_recording(
    db: Arc<DatabaseManager>,
    output_path: Arc<String>,
    fps: f64,
    audio_chunk_duration: Duration,
    vision_control: Arc<AtomicBool>,
    audio_devices_control: Arc<SegQueue<(AudioDevice, DeviceControl)>>,
    save_text_files: bool,
    audio_transcription_engine: Arc<AudioTranscriptionEngine>,
    ocr_engine: Arc<OcrEngine>,
    friend_wearable_uid: Option<String>,
    monitor_id: u32,
    use_pii_removal: bool,
) -> Result<()> {
    let (whisper_sender, whisper_receiver) =
        create_whisper_channel(audio_transcription_engine.clone()).await?;

    let db_manager_video = Arc::clone(&db);
    let db_manager_audio = Arc::clone(&db);

    let is_running_video = Arc::clone(&vision_control);

    let output_path_video = Arc::clone(&output_path);
    let output_path_audio = Arc::clone(&output_path);

    let friend_wearable_uid_video = friend_wearable_uid.clone();

    // Initialize friend wearable loop
    if let Some(uid) = &friend_wearable_uid {
        tokio::spawn(initialize_friend_wearable_loop(
            uid.clone(),
            Arc::clone(&db),
        ));
    }

    let video_handle = tokio::spawn(async move {
        record_video(
            db_manager_video,
            output_path_video,
            fps,
            is_running_video,
            save_text_files,
            ocr_engine,
            friend_wearable_uid_video,
            monitor_id,
            use_pii_removal,
        )
        .await
    });

    let audio_handle = tokio::spawn(async move {
        record_audio(
            db_manager_audio,
            output_path_audio,
            audio_chunk_duration,
            whisper_sender,
            whisper_receiver,
            audio_devices_control,
            friend_wearable_uid,
            audio_transcription_engine,
        )
        .await
    });

    let video_result = video_handle.await;
    let audio_result = audio_handle.await;

    if let Err(e) = video_result {
        error!("Video recording error: {:?}", e);
    }
    if let Err(e) = audio_result {
        error!("Audio recording error: {:?}", e);
    }

    info!("Stopped recording");
    Ok(())
}

async fn record_video(
    db: Arc<DatabaseManager>,
    output_path: Arc<String>,
    fps: f64,
    is_running: Arc<AtomicBool>,
    save_text_files: bool,
    ocr_engine: Arc<OcrEngine>,
    _friend_wearable_uid: Option<String>,
    monitor_id: u32,
    use_pii_removal: bool,
) -> Result<()> {
    debug!("record_video: Starting");
    let db_chunk_callback = Arc::clone(&db);
    let rt = tokio::runtime::Handle::current();
    let new_chunk_callback = move |file_path: &str| {
        let db_chunk_callback = Arc::clone(&db_chunk_callback);
        let file_path = file_path.to_string();
        rt.spawn(async move {
            if let Err(e) = db_chunk_callback.insert_video_chunk(&file_path).await {
                error!("Failed to insert new video chunk: {}", e);
            }
            debug!("record_video: Inserted new video chunk: {}", file_path);
        });
    };

    let video_capture = VideoCapture::new(
        &output_path,
        fps,
        new_chunk_callback,
        save_text_files,
        Arc::clone(&ocr_engine),
        monitor_id,
    );

    while is_running.load(Ordering::SeqCst) {
        if let Some(frame) = video_capture.ocr_frame_queue.pop() {
            for window_result in &frame.window_ocr_results {
                match db.insert_frame().await {
                    Ok(frame_id) => {
                        let text_json =
                            serde_json::to_string(&window_result.text_json).unwrap_or_default();

                        let text = if use_pii_removal {
                            &remove_pii(&window_result.text)
                        } else {
                            &window_result.text
                        };
                        if let Err(e) = db
                            .insert_ocr_text(
                                frame_id,
                                text,
                                &text_json,
                                &window_result.app_name,
                                &window_result.window_name,
                                Arc::clone(&ocr_engine),
                                window_result.focused, // Add this line
                            )
                            .await
                        {
                            error!(
                                "Failed to insert OCR text: {}, skipping window {} of frame {}",
                                e, window_result.window_name, frame_id
                            );
                            continue;
                        }
                    }
                    Err(e) => {
                        warn!("Failed to insert frame: {}", e);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs_f64(1.0 / fps)).await;
    }

    Ok(())
}

async fn record_audio(
    db: Arc<DatabaseManager>,
    output_path: Arc<String>,
    chunk_duration: Duration,
    whisper_sender: UnboundedSender<AudioInput>,
    mut whisper_receiver: UnboundedReceiver<TranscriptionResult>,
    audio_devices_control: Arc<SegQueue<(AudioDevice, DeviceControl)>>,
    friend_wearable_uid: Option<String>,
    audio_transcription_engine: Arc<AudioTranscriptionEngine>,
) -> Result<()> {
    let mut handles: HashMap<String, JoinHandle<()>> = HashMap::new();

    loop {
        while let Some((audio_device, device_control)) = audio_devices_control.pop() {
            debug!("Received audio device: {}", &audio_device);
            let device_id = audio_device.to_string();

            if !device_control.is_running {
                info!("Device control signaled stop for device {}", &audio_device);
                if let Some(handle) = handles.remove(&device_id) {
                    handle.abort();
                    info!("Stopped thread for device {}", &audio_device);
                }
                continue;
            }

            let output_path_clone = Arc::clone(&output_path);
            let whisper_sender_clone = whisper_sender.clone();

            let audio_device = Arc::new(audio_device);
            let device_control = Arc::new(device_control);

            let handle = tokio::spawn(async move {
                let audio_device_clone = Arc::clone(&audio_device);
                let device_control_clone = Arc::clone(&device_control);
                debug!(
                    "Starting audio capture thread for device: {}",
                    &audio_device
                );

                let mut iteration = 0;
                loop {
                    iteration += 1;
                    debug!(
                        "Starting iteration {} for device {}",
                        iteration, audio_device_clone
                    );

                    let output_path_clone = Arc::clone(&output_path_clone);
                    let whisper_sender = whisper_sender_clone.clone();
                    let audio_device_clone = audio_device_clone.clone();
                    let audio_device_clone_2 = audio_device_clone.clone();
                    let device_control_clone = device_control_clone.clone();

                    let new_file_name = Utc::now().format("%Y-%m-%d_%H-%M-%S").to_string();
                    let file_path = PathBuf::from(&*output_path_clone)
                        .join(format!("{}_{}.mp4", audio_device_clone, new_file_name))
                        .to_str()
                        .expect("Failed to create valid path")
                        .to_string();
                    debug!(
                        "Starting record_and_transcribe for device {} (iteration {})",
                        audio_device_clone, iteration
                    );
                    let result = record_and_transcribe(
                        audio_device_clone,
                        chunk_duration,
                        file_path.into(),
                        whisper_sender,
                        Arc::new(AtomicBool::new(device_control_clone.is_running)),
                    )
                    .await;
                    info!(
                        "Finished record_and_transcribe for device {} (iteration {})",
                        audio_device_clone_2, iteration
                    );

                    match result {
                        Ok(file_path) => {
                            info!(
                                "Recording complete for device {} (iteration {}): {:?}",
                                audio_device, iteration, file_path
                            );
                        }
                        Err(e) => {
                            error!(
                                "Error in record_and_transcribe for device {} (iteration {}): {}, stopping thread",
                                audio_device, iteration, e
                            );
                            break;
                        }
                    }

                    info!(
                        "Finished iteration {} for device {}",
                        iteration, &audio_device
                    );
                }

                info!("Exiting audio capture thread for device: {}", &audio_device);
            });

            handles.insert(device_id, handle);
        }

        handles.retain(|device_id, handle| {
            if handle.is_finished() {
                info!("Handle for device {} has finished", device_id);
                false
            } else {
                true
            }
        });

        while let Ok(transcription) = whisper_receiver.try_recv() {
            info!("Received transcription");
            // avoiding crashing the audio processing if one fails
            if let Err(e) = process_audio_result(
                &db,
                transcription,
                friend_wearable_uid.as_deref(),
                audio_transcription_engine.clone(),
            )
            .await
            {
                error!("Error processing audio result: {}", e);
                // Optionally, you can add more specific error handling here
            }
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn process_audio_result(
    db: &DatabaseManager,
    result: TranscriptionResult,
    _friend_wearable_uid: Option<&str>,
    audio_transcription_engine: Arc<AudioTranscriptionEngine>,
) -> Result<(), anyhow::Error> {
    if result.error.is_some() || result.transcription.is_none() {
        error!(
            "Error in audio recording: {}. Not inserting audio result",
            result.error.unwrap_or_default()
        );
        return Ok(());
    }
    let transcription = result.transcription.unwrap();
    let transcription_engine = audio_transcription_engine.to_string();

    info!("Inserting audio chunk: {:?}", result.input.path);
    match db.insert_audio_chunk(&result.input.path).await {
        Ok(audio_chunk_id) => {
            if transcription.is_empty() {
                return Ok(());
            }

            if let Err(e) = db
                .insert_audio_transcription(
                    audio_chunk_id,
                    &transcription,
                    0,
                    &transcription_engine,
                )
                .await
            {
                error!(
                    "Failed to insert audio transcription for device {}: {}",
                    result.input.device, e
                );
                return Ok(());
            } else {
                debug!(
                    "Inserted audio transcription for chunk {} from device {} using {}",
                    audio_chunk_id, result.input.device, transcription_engine
                );
            }
        }
        Err(e) => error!(
            "Failed to insert audio chunk for device {}: {}",
            result.input.device, e
        ),
    }
    Ok(())
}
