use std::fmt::Debug;
use std::sync::Arc;

use debug_print::debug_println;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::{ChannelReceiver, ChannelSender, WebhookMessage};

/// Maximum number of retries for failed requests
const MAX_RETRIES: usize = 10;

/// This worker manages a background async task that schedules the network requests to send traces
/// to the webhook on the running tokio runtime.
///
/// Ensure to invoke `.start()` before, and `.teardown()` after, your application code runs. This
/// is required to ensure proper initialization and shutdown.
///
/// `tracing-layer-core` synchronously generates payloads to send to the webhook using the
/// tracing events from the global subscriber. However, all network requests are offloaded onto
/// an unbuffered channel and processed by a provided future acting as an asynchronous worker.
#[derive(Clone)]
pub struct BackgroundWorker {
    /// The sender used to send messages to the worker task.
    ///
    /// This sender is used to send `WorkerMessage` instances to the worker for processing.
    pub(crate) sender: ChannelSender,

    /// A handle to the spawned worker task.
    ///
    /// This handle is used to await the completion of the worker task when shutting down.
    /// The handle is stored in a `tokio::sync::Mutex` to ensure safe access across asynchronous contexts.
    pub(crate) handle: Arc<Mutex<Option<JoinHandle<()>>>>,

    /// The receiver for messages to be processed by the worker task.
    ///
    /// This receiver is wrapped in an `Arc<Mutex<>>` to allow shared mutable access
    /// between the `start` function and the worker task.
    pub(crate) rx: Arc<Mutex<ChannelReceiver>>,
}

impl BackgroundWorker {
    /// Starts the background worker.
    ///
    /// This function should only be called once. Attempting to call `start` more than once
    /// will lead to a deadlock, as the function internally locks the receiver mutex and
    /// spawns a task to process messages.
    pub async fn start(&self) {
        let rx = self.rx.clone();
        let future = async move {
            let mut rx = rx.lock().await;
            worker(&mut *rx).await;
        };
        let handle = tokio::spawn(future);
        let mut guard = self.handle.lock().await;
        *guard = Some(handle);
    }

    /// Initiates the shutdown of the background worker.
    ///
    /// Sends a shutdown message to the worker and waits for the worker task to complete.
    /// If the worker task handle has already been dropped, an error message will be printed.
    pub async fn shutdown(self) {
        match self.sender.send(WorkerMessage::Shutdown) {
            Ok(..) => {
                debug_println!("webhook message worker shutdown");
            }
            Err(e) => {
                #[cfg(feature = "log-errors")]
                eprintln!(
                    "ERROR: failed to send shutdown message to webhook message worker: {}",
                    e
                );
            }
        }
        let mut guard = self.handle.lock().await;
        if let Some(handle) = guard.take() {
            let _ = handle.await;
        } else {
            #[cfg(feature = "log-errors")]
            eprintln!("ERROR: async task handle to webhook message worker has been already dropped");
        }
    }
}

/// A command sent to a worker containing a new message that should be sent to a webhook endpoint.
#[derive(Debug)]
pub enum WorkerMessage {
    Data(Box<dyn WebhookMessage>),
    Shutdown,
}

/// Provides a background worker task that sends the messages generated by the layer.
pub(crate) async fn worker(rx: &mut ChannelReceiver) {
    let client = reqwest::Client::new();
    while let Some(message) = rx.recv().await {
        match message {
            WorkerMessage::Data(payload) => {
                let webhook_url = payload.webhook_url();
                let payload_json = payload.serialize();
                debug_println!("sending webhook message: {}", &payload_json);

                let mut retries = 0;
                while retries < MAX_RETRIES {
                    match client
                        .post(webhook_url)
                        .header("Content-Type", "application/json")
                        .body(payload_json.clone())
                        .send()
                        .await
                    {
                        Ok(res) => {
                            debug_println!("webhook message sent: {:?}", &res);
                            let res_text = res.text().await.unwrap();
                            debug_println!("webhook message response: {}", res_text);
                            break; // Success, break out of the retry loop
                        }
                        Err(e) => {
                            #[cfg(feature = "log-errors")]
                            eprintln!("ERROR: failed to send webhook message: {}", e);
                        }
                    };

                    // Exponential backoff - increase the delay between retries
                    let delay_ms = 2u64.pow(retries as u32) * 100;
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    retries += 1;
                }
            }
            WorkerMessage::Shutdown => {
                break;
            }
        }
    }
}
