mod web_server;

use web_server::{run_ws_server, AudioTx};

use std::io::Cursor;
use moq_native::quic;
use tokio::time::{sleep, timeout, Duration};
use std::net;

use url::Url;

use anyhow::Context;
use clap::Parser;
use moq_transfork::*;
use std::fs::File as StdFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use rodio::{Decoder, OutputStream, Sink, Source};
use opus::{Channels, Encoder as OpusEncoder, Application};
use std::io::BufReader;
use std::process::Command;
use opus::Decoder as OpusDecoder;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use rodio::cpal::FromSample;
use tokio::fs::File;

#[derive(Parser, Clone)]
pub struct Config {
    /// Listen for UDP packets on the given address.
    #[arg(long, default_value = "[::]:0")]
    pub bind: net::SocketAddr,

    /// Connect to the given URL starting with https://
    #[arg()]
    pub url: Url,

    /// The TLS configuration.
    #[command(flatten)]
    pub tls: moq_native::tls::Args,

    /// The path of the clock track.
    #[arg(long, default_value = "clock")]
    pub path: String,

    /// The log configuration.
    #[command(flatten)]
    pub log: moq_native::log::Args,

    /// Whether to publish the clock or consume it.
    #[command(subcommand)]
    pub role: Cmd,

    #[arg(long)]
    pub station_index: u16,
}

// Possible CMD Line arguments
#[derive(Parser, Clone)]
pub enum Cmd {
    Publish,
    Subscribe,
}




pub async fn pub_opus_from_mp3(path: &str, mut writer: TrackProducer) -> anyhow::Result<()> {
    let file = StdFile::open(path)?;
    let source = Decoder::new(BufReader::new(file))?;
    // test_audio_source_encoding(source);
    let sample_rate = source.sample_rate() as u32;
    let channel_count = source.channels() as usize;
    println!("{}", channel_count);// e.g. 2
    // e.g. 44100
    let channels = match channel_count {
        1 => Channels::Mono,
        2 => Channels::Stereo,
        n => anyhow::bail!("unsupported channel count: {}", n),
    };

    // 1.3) Create your encoder with the *actual* rate & channels
    let supported_sample_rate = 48000; // Default supported sample rate for Opus
    if sample_rate != supported_sample_rate {
        eprintln!(
            "Sample rate of {} Hz is not supported. Resampling to {} Hz.",
            sample_rate, supported_sample_rate
        );
        //FIXME
        // Add resampling logic here if your sample rate is not supported
    }
    // Pass `supported_sample_rate` to the encoder instead of `sample_rate`
    let mut encoder = OpusEncoder::new(supported_sample_rate, channels, Application::Audio)?;

    // 1.4) Compute how many i16 samples you need per frame:
    //     Opus expects “frames per channel.” 20 ms = rate/50.
    let frame_size = (supported_sample_rate / 1000 * 20) as usize;         // e.g. 882 samples
    let samples_per_frame = frame_size * channel_count;         // e.g. 882*2 = 1764 i16s

    let mut pcm_buffer = Vec::with_capacity(samples_per_frame);
    println!("{}", pcm_buffer.capacity());
    let mut sequence = 0;

    // Collect samples and encode them into Opus frames
    // create exactly one group (stream) for the whole song
    let mut group = writer.create_group(0);

    // set up a fixed-interval ticker for real-time pacing
    let mut ticker = tokio::time::interval(Duration::from_millis(20));
    ticker.tick().await; // drop initial immediate tick

    for sample in source.convert_samples::<i16>() {
        pcm_buffer.push(sample);

        if pcm_buffer.len() ==  1920{
            // encode
            let mut output = [0u8; 4000];
            let len = encoder.encode(&pcm_buffer, &mut output)?;
            let payload = &output[..len];

            // length-prefix
            let mut buf = BytesMut::with_capacity(4 + len);
            buf.put_u32(len as u32);
            buf.put_slice(payload);

            // wait for next 20 ms slot
            ticker.tick().await;
            group.write_frame(buf.freeze());

            pcm_buffer.clear();
        }
    }

    // signal EOF
    drop(group);

    Ok(())
}

pub async fn sub_play_opus(mut reader: TrackConsumer, tx: AudioTx) -> anyhow::Result<()> {
    let sample_rate = 48000;
    let channels = Channels::Stereo;
    let mut decoder = OpusDecoder::new(sample_rate, channels)?;
    let mut pcm_buf = [0i16; 960 * 2];


    loop {
        match timeout(Duration::from_secs(5), reader.next_group()).await {
            Ok(Ok(Some(mut group))) => {
                println!("Received new group! Starting to process frames...");

                while let Some(mut frame) = group.next_frame().await? {
                    // 1. Collect the frame
                    let mut full = BytesMut::new();
                    while let Ok(Some(chunk)) = frame.read().await {
                        full.extend_from_slice(&chunk);
                    }

                    // 2. Sanity check: Must have at least 4 bytes for packet length
                    if full.len() < 4 {
                        eprintln!("Frame too small: {} bytes", full.len());
                        continue;
                    }

                    // 3. Read length-prefixed Opus packet
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

                    let packet = &full[4..4 + packet_len];

                    // 4. Decode Opus packet into PCM samples
                    let samples = decoder.decode(packet, &mut pcm_buf, false)?;
                    let pcm_bytes = bytemuck::cast_slice(&pcm_buf[..samples * 2]).to_vec();

                    // 5. Send PCM samples to WebSocket clients
                    let _ = tx.send(pcm_bytes);
                }
            }
            Ok(Ok(None)) => {
                println!("Track ended cleanly (no more groups).");
                break;
            }
            Ok(Err(e)) => {
                eprintln!("Error while getting next group: {:?}", e);
                break;
            }
            Err(_) => {
                println!("Timeout while waiting for new group. Assuming track is unavailable.");
                break;
            }
        }
    }


    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    config.log.init();

    let tls = config.tls.load()?;
    let quic = quic::Endpoint::new(quic::Config { bind: config.bind, tls })?;
    let session = quic.client.connect(config.url).await?;
    let mut session = moq_transfork::Session::connect(session).await?;

    let (tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(100);

    match config.role {
        Cmd::Publish => {
            let mut playlist = vec![];

            if config.station_index == 1 {
                playlist.extend([
                    "angels", "truth", "echo", "linger",
                    "dawn", "fireside", "hope", "soulsweeper"
                ].iter().map(|s| s.to_string()));

            } else if config.station_index == 2 {

                playlist.extend([
                    "dreams", "sad", "Midnight_Memories", "villain",
                    "yesterday", "hope", "fireside", "echo"
                ].iter().map(|s| s.to_string()));

            } else if config.station_index == 3 {

                playlist.extend([
                    "truth", "soulsweeper", "angels", "dawn",
                    "dreams", "sad", "linger", "Midnight_Memories"
                ].iter().map(|s| s.to_string()));

            }

            let mut loop_counter = 0;

            loop {
                for (i, song) in playlist.iter().enumerate() {
                    let track_name = format!("station{}-{}-{}", config.station_index, loop_counter, i);
                    println!("Publishing new track: {}", track_name);

                    let track = Track::new(track_name.clone());
                    let (writer, reader) = track.produce();

                    session.publish(reader.clone()).context("failed to announce broadcast")?;

                    let song_path = format!("songs/{}.mp3", song);

                    File::open(&song_path).await
                        .with_context(|| format!("could not open song: {}", song))?;

                    pub_opus_from_mp3(&song_path, writer.clone()).await?;

                    println!("Finished song: {}", song);
                }
                loop_counter += 1;
                println!("Playlist loop complete. Restarting...");
            }

        }

        Cmd::Subscribe => {

            let station_id = format!("station{}", config.station_index);
            let port = 3030 + config.station_index - 1;
            println!("Starting WebSocket server on port {}", port);

            tokio::spawn(run_ws_server(tx.clone(), station_id, port));

            let mut song_index = 0;
            let mut loop_counter = 0;
            let playlist_len = 5;

            loop {
                let track_name = format!("station{}-{}-{}", config.station_index, loop_counter, song_index);
                println!("Subscribing to track: {}", track_name);

                let track = Track::new(track_name.clone());
                let reader = session.subscribe(track.clone());

                match sub_play_opus(reader, tx.clone()).await {
                    Ok(_) => {
                        println!("Finished playing track: {}", track_name);
                    }
                    Err(e) => {
                        eprintln!("Error playing track: {:?}. Retrying in 5s...", e);
                        sleep(Duration::from_secs(5)).await;
                    }
                }

                println!("Waiting 2 seconds before trying next track...");
                sleep(Duration::from_secs(2)).await;

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
