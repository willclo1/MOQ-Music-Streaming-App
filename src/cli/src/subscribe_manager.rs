//! Manages subscriber connections by spawning and controlling external processes.
//
// This module is responsible for starting, monitoring, and stopping a subscriber process
// that connects to a live audio streaming station. It interacts with the CLI binary
// and reports status updates to the UI via channels.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Manages the lifecycle of a subscriber process for a given station.
pub struct SubscriberManager {
    child: Option<Child>, // Handle to the spawned subscriber process
}

impl SubscriberManager {
    /// Creates a new instance of the SubscriberManager with no active child process.
    pub fn new() -> Self {
        SubscriberManager { child: None }
    }

    /// Connects to a radio station by spawning a new subscriber process.
    ///
    /// - `station_index`: The index of the station to connect to.
    /// - `url`: The URL of the host server.
    /// - `status_tx`: A sender used to communicate status updates back to the UI.
    ///
    /// This function kills any existing subscriber, starts a new one,
    /// and spawns a thread to listen for success/failure handshake from its stdout.
    pub fn connect(&mut self, station_index: u16, url: &str, status_tx: Sender<String>) -> Result<()> {
        // Terminate any running subscriber process before spawning a new one.
        let _ = self.disconnect();

        let station_arg = station_index.to_string();

        // Spawn a new subscriber process using the CLI binary and target arguments.
        let mut child = Command::new("cargo")
            .args(&[
                "run", "--bin", "final-project-group3_s25", "--",
                "--station-index", &station_arg,
                url,
                "subscribe",
            ])
            .stdout(Stdio::piped()) // Capture stdout for status updates
            .stderr(Stdio::piped()) // Capture stderr for debugging
            .spawn()
            .context("failed to spawn subscriber process")?;

        // Begin processing the output of the spawned subscriber
        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            let tx_clone = status_tx.clone();
            let url_string = url.to_string();
            let station_arg_clone = station_arg.clone();

            // Spawn a separate thread to watch subscriber's stdout for confirmation or errors
            thread::spawn(move || {
                let timeout = Instant::now() + Duration::from_secs(10); // Set a timeout for handshake
                let mut handshake_done = false;
                let mut lines = Vec::new(); // Store initial output lines for diagnostics

                for maybe_line in reader.lines() {
                    if let Ok(line) = maybe_line {
                        // Capture all lines until handshake is confirmed or times out
                        if !handshake_done {
                            lines.push(line.clone());
                            if line.contains("🎧 New group started") {
                                // Successful connection established
                                let _ = tx_clone.send(format!(
                                    "▶️ Connected to station {} at {}",
                                    station_arg_clone, url_string
                                ));
                                handshake_done = true;
                                continue; // Continue reading to avoid pipe blocking
                            }

                            // If no handshake occurs in time, break and report error
                            if Instant::now() > timeout {
                                break;
                            }
                        }
                        // After handshake, continue draining output to prevent pipe blocking
                    }
                }

                // If handshake never completed, analyze captured output for specific error hints
                if !handshake_done {
                    let error_detected = lines.iter().any(|l| {
                        l.contains("No group received")
                            || l.contains("subscribe error")
                            || l.contains("Error...no publisher/relay connection")
                    });

                    // Report appropriate error message to UI
                    if error_detected {
                        let _ = tx_clone.send("Station does not exist or failed to subscribe".into());
                    } else {
                        let _ = tx_clone.send("Disconnected...try connect again".into());
                    }
                }
            });
        }

        self.child = Some(child); // Store the child process handle
        Ok(())
    }

    /// Disconnects any running subscriber process by killing and reaping it.
    ///
    /// This function attempts to terminate the currently running subscriber process,
    /// waits for it to exit cleanly, and logs the disconnection event.
    pub fn disconnect(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            // Attempt to kill the process
            child.kill().context("failed to kill subscriber process")?;
            // Wait for it to finish exiting
            child.wait().context("failed to reap subscriber process")?;
            println!("❌ Disconnected subscriber");
        }
        Ok(())
    }
}