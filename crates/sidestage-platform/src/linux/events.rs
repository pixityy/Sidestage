//! Server events (sink-input, sink and server changes) on a dedicated subscribed connection,
//! forwarded to the session thread. Stream lifecycle is tracked through these events rather
//! than through stream callbacks, so nothing is missed when a stream is killed or moved.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use anyhow::Context;
use pulseaudio::protocol::{Command, SubscriptionMask};

use super::conn::{CONTROL, Conn, Control};
use super::session::Msg;

pub struct EventWatch {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl EventWatch {
    pub fn start(tx: Sender<Msg>) -> anyhow::Result<Self> {
        let mut conn = Conn::connect("events")?;
        conn.ack(&Command::Subscribe(SubscriptionMask::SINK_INPUT | SubscriptionMask::SINK | SubscriptionMask::SERVER))
            .context("subscribing to sound server events")?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("sidestage-events".into())
            .spawn(move || {
                if let Err(e) = watch(&mut conn, &flag, &tx) {
                    let _ = tx.send(Msg::EventsLost(format!("{e:#}")));
                }
            })
            .context("starting the event thread")?;
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for EventWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            log::error!("pulse: the event thread panicked");
        }
    }
}

fn watch(conn: &mut Conn, stop: &AtomicBool, tx: &Sender<Msg>) -> anyhow::Result<()> {
    while !stop.load(Ordering::Relaxed) {
        if !conn.wait_readable()? {
            continue;
        }
        let header = conn.read_header()?;
        if header.channel != CONTROL {
            conn.skip(header.length)?;
            continue;
        }
        conn.read_control(header.length)?;
        match conn.decode() {
            Ok(Control::Command(Command::SubscribeEvent(event))) => {
                if tx.send(Msg::Event(event)).is_err() {
                    return Ok(()); // the session is gone
                }
            }
            Ok(_) => {}
            Err(e) => log::debug!("pulse: event connection ignored a message: {e}"),
        }
    }
    Ok(())
}
