use moq_transfork::TrackProducer;
use tokio::time::Duration;
use moq_transfork::*;
use std::fs::File as StdFile;
use rodio::{Decoder, Source};
use opus::{Channels, Encoder as OpusEncoder, Application};
use std::io::BufReader;
use std::net;
use anyhow::Context;
use bytes::{BufMut, BytesMut};
use clap::Parser;
use tokio::fs::File;
use url::Url;

/// Command-line config for the MoQ audio application.
#[derive(Parser, Clone)]
pub struct Config {
    /// Listen for UDP packets on the given address (used by QUIC).
    #[arg(long, default_value = "[::]:0")]
    pub bind: net::SocketAddr,

    /// Connect to the given URL starting with https:// (the MoQ relay address).
    #[arg()]
    pub url: Url,

    /// The TLS configuration for QUIC.
    #[command(flatten)]
    pub tls: moq_native::tls::Args,

    /// The path of the clock track (not actively used here).
    #[arg(long, default_value = "clock")]
    pub path: String,

    /// The log configuration.
    #[command(flatten)]
    pub log: moq_native::log::Args,

    /// Whether to publish or subscribe.
    #[command(subcommand)]
    pub role: Cmd,

    /// Station index (used to pick a playlist).
    #[arg(long)]
    pub station_index: u16,
}

/// Enum indicating application mode: publisher or subscriber.
#[derive(Parser, Clone)]
pub enum Cmd {
    /// Publish audio tracks.
    Publish,

    /// Subscribe to audio tracks.
    Subscribe,
}

/// Encodes an MP3 file into Opus and streams it using a MoQ `TrackProducer`.
///
/// This function:
/// - Loads an MP3 file from the filesystem.
/// - Decodes it into PCM samples using rodio.
/// - Encodes samples into 20ms Opus frames using the opus crate.
/// - Streams each frame in real-time using a 20ms interval timer.
///
/// # Arguments
/// * path - The path to the .mp3 file to stream (e.g. "songs/track1.mp3").
/// * writer - A `rackProducer to which encoded Opus frames will be written.
///
/// # Returns
/// A result indicating success or failure.
pub async fn pub_opus_from_mp3(path: &str, mut writer: TrackProducer) -> anyhow::Result<()> {
    let file = StdFile::open(path)?;
    let source = Decoder::new(BufReader::new(file))?;

    let sample_rate = source.sample_rate();
    let channel_count = source.channels() as usize;
    println!("{}", channel_count); // Print detected number of channels

    // Determine Opus channel configuration
    let channels = match channel_count {
        1 => Channels::Mono,
        2 => Channels::Stereo,
        n => anyhow::bail!("unsupported channel count: {}", n),
    };

    // Opus requires 48000 Hz sample rate
    let supported_sample_rate = 48000;
    if sample_rate != supported_sample_rate {
        eprintln!(
            "Sample rate of {} Hz is not supported. Resampling to {} Hz.",
            sample_rate, supported_sample_rate
        );
        // FIXME: This branch warns but does not resample (yet)
    }

    // Create Opus encoder
    let mut encoder = OpusEncoder::new(supported_sample_rate, channels, Application::Audio)?;

    // 20 ms frame = 960 samples per channel at 48kHz
    let frame_size = (supported_sample_rate / 1000 * 20) as usize;
    let samples_per_frame = frame_size * channel_count;

    let mut pcm_buffer = Vec::with_capacity(samples_per_frame);
    println!("{}", pcm_buffer.capacity()); // Print frame buffer size

    // Create a group to hold this song's audio frames
    let mut group = writer.create_group(0);

    // Use a fixed interval to send audio frames every 20ms
    let mut ticker = tokio::time::interval(Duration::from_millis(20));
    ticker.tick().await; // Drop the immediate tick

    for sample in source.convert_samples::<i16>() {
        pcm_buffer.push(sample);

        if pcm_buffer.len() == 1920 {
            // Encode PCM samples to Opus
            let mut output = [0u8; 4000];
            let len = encoder.encode(&pcm_buffer, &mut output)?;
            let payload = &output[..len];

            // Prefix frame with 4-byte length
            let mut buf = BytesMut::with_capacity(4 + len);
            buf.put_u32(len as u32);
            buf.put_slice(payload);

            // Wait for next 20ms slot and write frame
            ticker.tick().await;
            group.write_frame(buf.freeze());

            pcm_buffer.clear();
        }
    }

    drop(group); // Close track group
    Ok(())
}

/// Publishes an audio playlist (based on station index) to a MoQ relay,
/// encoding each track to Opus and synchronizing playback with metadata timestamps.
///
/// This function:
/// - Selects a playlist based on config.station_index
/// - Publishes each song as a MoQ track
/// - Attaches a metadata track that sends timestamps every 20ms
///
/// # Arguments
/// * config - CLI configuration specifying bind address, station index, relay URL, etc.
/// * session - An active MoQ Session for publishing tracks and metadata to the relay.
///
/// # Returns
/// A result indicating success or failure of the publish loop.
///
pub async fn perform_pub_cmd(config: Config, mut session: Session) -> anyhow::Result<()> {
    // Keep track of current metadata background task (so we can cancel it)
    let mut metadata_handle: Option<tokio::task::JoinHandle<()>> = None;

    let mut playlist = vec![];

    // Choose playlist based on station index
    if config.station_index == 1 {
        playlist.extend(["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()));
    } else if config.station_index == 2 {
        playlist.extend([
            "dreams", "sad", "Midnight_Memories", "villain", "yesterday",
        ].iter().map(|s| s.to_string()));
    } else if config.station_index == 3 {
        playlist.extend(["truth", "soulsweeper", "angels", "dawn", "dreams"]
            .iter()
            .map(|s| s.to_string()));
    }

    let mut loop_counter = 0;

    loop {
        for (i, song) in playlist.iter().enumerate() {
            // Construct unique track name using station, loop, and song index
            let track_name = format!("station{}-{}-{}", config.station_index, loop_counter, i);
            println!("Publishing new track: {}", track_name);

            // Create and announce new audio track
            let track = Track::new(track_name.clone());
            let (writer, reader) = track.produce();
            session.publish(reader.clone()).context("failed to announce broadcast")?;

            // Abort previous metadata task (if any)
            if let Some(handle) = metadata_handle.take() {
                handle.abort();
            }

            // Create and announce new metadata track for timestamps
            let metadata_track = Track::new(format!("metadata-{}", track_name));
            let (md_writer, md_reader) = metadata_track.produce();
            session.publish(md_reader.clone()).context("failed to announce metadata track")?;

            // Spawn async task to stream elapsed time to metadata track
            metadata_handle = Some(tokio::spawn({
                let mut md_writer = md_writer.clone();
                async move {
                    let mut group_id: u64 = 0;
                    let mut ticker = tokio::time::interval(Duration::from_millis(20));
                    ticker.tick().await;
                    let start_time = std::time::Instant::now();

                    loop {
                        ticker.tick().await;
                        let elapsed_ms = start_time.elapsed().as_millis() as u64;
                        let mut buf = BytesMut::with_capacity(8);
                        buf.put_u64(elapsed_ms);
                        let mut group = md_writer.create_group(group_id);
                        group.write_frame(buf.freeze());
                        group_id = group_id.wrapping_add(1);
                    }
                }
            }));

            // Verify file exists before trying to stream it
            let song_path = format!("songs/{}.mp3", song);
            File::open(&song_path)
                .await
                .with_context(|| format!("could not open song: {}", song))?;

            // Encode and stream this MP3 to the current track
            pub_opus_from_mp3(&song_path, writer.clone()).await?;

            println!("Finished song: {}", song);
        }

        loop_counter += 1;
        println!("Playlist loop complete. Restarting...");
    }
}