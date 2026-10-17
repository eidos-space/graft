use std::{
    io::{self, IsTerminal, Write},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::Duration,
};

/// The first frame is flushed before repository initialization. Dropping the
/// guard joins the renderer and clears its line, including on startup errors.
pub(super) struct StartupLoading {
    animation: Option<(mpsc::Sender<()>, JoinHandle<()>)>,
}

impl StartupLoading {
    pub(super) fn start() -> Self {
        let animated = io::stderr().is_terminal()
            && std::env::var_os("TERM").is_none_or(|term| term != "dumb");
        if !animated {
            let _ = writeln!(io::stderr(), "Opening Graft…");
            return Self { animation: None };
        }
        draw_frame(0);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut frame = 0;
            while matches!(
                receiver.recv_timeout(Duration::from_millis(80)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                frame = (frame + 1) % 4;
                draw_frame(frame);
            }
        });
        Self { animation: Some((sender, worker)) }
    }
}

fn draw_frame(frame: usize) {
    const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
    let mut stderr = io::stderr().lock();
    let _ = write!(stderr, "\r{} Opening Graft…", FRAMES[frame]);
    let _ = stderr.flush();
}

impl Drop for StartupLoading {
    fn drop(&mut self) {
        if let Some((sender, worker)) = self.animation.take() {
            let _ = sender.send(());
            let _ = worker.join();
            let mut stderr = io::stderr().lock();
            let _ = write!(stderr, "\r\x1b[2K");
            let _ = stderr.flush();
        }
    }
}
