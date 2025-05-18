use std::io::Cursor;
use moq_native::quic;
use clap::Parser;
use moq_transfork::*;
use rodio::{OutputStream, Sink};
use opus::{Channels, Decoder as OpusDecoder};
use bytes::{Buf, BytesMut};
use final_project_group3_s25::{perform_pub_cmd, Cmd, Config};

/// Subscribe to and play Opus audio from a given track and its corresponding metadata track.
///
/// # Arguments
/// * reader - TrackConsumer for the main audio stream.
/// * metadata_reader - TrackConsumer for metadata stream (used to synchronize audio).
pub async fn sub_play_opus(mut reader: TrackConsumer, mut metadata_reader: TrackConsumer) -> anyhow::Result<()> {
    // Read the initial timestamp from the metadata stream
    let initial_timestamp = match metadata_reader.next_group().await {
        Ok(Some(mut group)) => {
            if let Ok(Some(mut frame)) = group.next_frame().await {
                let mut buf_vec = Vec::new();
                while buf_vec.len() < 8 {
                    match frame.read().await {
                        Ok(Some(chunk)) => buf_vec.extend_from_slice(&chunk),
                        _ => break,
                    }
                }
                // Get the metadata
                if buf_vec.len() >= 8 {
                    let mut arr = [0u8; 8];
                    arr.copy_from_slice(&buf_vec[..8]);
                    u64::from_be_bytes(arr)
                } else {
                    0
                }
            } else {
                0
            }
        }
        _ => 0,
    };

    // Spawn a background task to continue printing metadata timestamps
    let mut md_reader = metadata_reader.clone();
    tokio::spawn(async move {
        loop {
            match md_reader.next_group().await {
                Ok(Some(mut group)) => {
                    if let Ok(Some(mut frame)) = group.next_frame().await {
                        let mut buf_vec = Vec::new();
                        while buf_vec.len() < 8 {
                            match frame.read().await {
                                Ok(Some(chunk)) => buf_vec.extend_from_slice(&chunk),
                                Ok(None) => break,
                                Err(e) => {
                                    eprintln!("❌ Error reading metadata chunk: {:?}", e);
                                    break;
                                }
                            }
                        }
                        if buf_vec.len() >= 8 {
                            let mut arr = [0u8; 8];
                            arr.copy_from_slice(&buf_vec[..8]);
                            let timestamp_ms = u64::from_be_bytes(arr);
                            // Optional: print or log timestamp_ms if needed
                        } else {
                            eprintln!("⚠️ Incomplete metadata frame: {} bytes", buf_vec.len());
                        }
                    }
                }
                Ok(None) => continue, // No metadata group yet
                Err(e) => {
                    eprintln!("❌ Error receiving metadata: {:?}", e);
                    break;
                }
            }
        }
    });

    // Setup Opus decoder and audio sink
    let sample_rate = 48000;
    let channels = Channels::Stereo;
    let mut decoder = OpusDecoder::new(sample_rate, channels)?;
    let mut pcm_buf = [0i16; 960 * 2];
    let (_stream, stream_handle) = OutputStream::try_default()?;
    let sink = Sink::try_new(&stream_handle)?;

    // Drop audio frames until the initial publisher timestamp is reached
    let mut dropped_ms = 0u64;
    let mut cur_group_opt = reader.next_group().await?;
    // Dropping the audio frames begins here
    while dropped_ms < initial_timestamp {
        if let Some(ref mut group) = cur_group_opt {
            while dropped_ms < initial_timestamp {
                if let Some(mut frame) = group.next_frame().await? {
                    let mut buf = BytesMut::new();
                    while let Ok(Some(chunk)) = frame.read().await {
                        buf.extend_from_slice(&chunk);
                    }
                    // gets the metadata packets and calculates the time in ms to drop
                    if buf.len() < 4 { continue; }
                    let mut cursor = Cursor::new(&buf[..]);
                    let packet_len = cursor.get_u32() as usize;
                    let packet = &buf[4..4 + packet_len];
                    let samples = decoder.decode(packet, &mut pcm_buf, false)?;
                    let frame_duration_ms = samples as u64 * 1000 / sample_rate as u64;
                    dropped_ms += frame_duration_ms;
                } else {
                    break;
                }
            }
        } else {
            break;
        }
        if dropped_ms < initial_timestamp {
            cur_group_opt = reader.next_group().await?;
        }
    }

    // Begin playback from the next valid group
    if let Some(mut group) = cur_group_opt {
        println!("🎧 New group started");
        while let Some(mut frame) = group.next_frame().await? {
            let mut full = BytesMut::new();
            while let Ok(Some(chunk)) = frame.read().await {
                full.extend_from_slice(&chunk);
            }
            if full.len() < 4 {
                eprintln!("Frame too small: {} bytes", full.len());
                continue;
            }
            let mut cursor = Cursor::new(&full[..]);
            let packet_len = cursor.get_u32() as usize;
            if full.len() < 4 + packet_len {
                eprintln!(
                    "Declared {} bytes but only {} available",
                    packet_len,
                    full.len() - 4
                );
                continue;
            }

            // Play the audio
            let packet = &full[4..4 + packet_len];
            let samples = decoder.decode(packet, &mut pcm_buf, false)?;
            let source = rodio::buffer::SamplesBuffer::new(2, sample_rate, &pcm_buf[..samples * 2]);
            sink.append(source);
        }
    } else {
        println!("⚠️ No group received");
    }

    // Wait for audio sink to finish playback
    sink.sleep_until_end();
    Ok(())
}

/// Entry point for publishing and subscribing
///
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    config.log.init();

    // Setup QUIC client connection
    let tls = config.tls.load()?;
    let quic = quic::Endpoint::new(quic::Config { bind: config.bind, tls })?;
    let session = quic.client.connect(config.url.clone()).await?;
    let mut session = Session::connect(session).await?;

    match config.role {
        Cmd::Publish => {
            // Handle publisher mode
            perform_pub_cmd(config, session).await?;
        }
        Cmd::Subscribe => {
            // Handle subscriber mode
            let port = 3030 + config.station_index - 1;
            println!("Starting WebSocket server on port {}", port);

            let mut song_index = 0;
            let mut loop_counter = 0;
            let playlist_len = 5;

            // Loop through the playlist continuously
            loop {
                let track_name = format!("station{}-{}-{}", config.station_index, loop_counter, song_index);
                println!("Subscribing to track: {}", track_name);

                let track = Track::new(track_name.clone());
                let reader = session.subscribe(track.clone());
                let metadata_reader = session.subscribe(Track::new(format!("metadata-{}", track_name)));

                match sub_play_opus(reader, metadata_reader).await {
                    Ok(_) => {
                        println!("Finished playing track: {}", track_name);
                    }
                    Err(e) => {
                        eprintln!("Error playing track: {:?}. Retrying in 5s...", e);
                    }
                }

                // Update playlist indices
                song_index += 1;
                if song_index >= playlist_len {
                    song_index = 0;
                    loop_counter += 1;
                }
            }
        }
    }

    Ok(())
}