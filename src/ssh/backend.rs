//! A ratatui backend that renders into an ssh channel.
//!
//! crossterm already knows how to turn a frame into escape sequences for any
//! writer, so that part is reused wholesale. What has to be replaced is
//! everything that asks the *local* terminal a question: the window size comes
//! from the ssh pty request, and the cursor position is tracked here rather
//! than queried, because querying would wait on a reply from a terminal that
//! is at the other end of the network.

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use tokio::sync::mpsc;

/// The pty size, shared with the ssh handler so a window change is visible to
/// the next draw.
#[derive(Debug, Default)]
pub struct PtySize {
    columns: AtomicU16,
    rows: AtomicU16,
}

impl PtySize {
    pub fn new(columns: u16, rows: u16) -> Arc<PtySize> {
        let size = Arc::new(PtySize::default());
        size.set(columns, rows);
        size
    }

    pub fn set(&self, columns: u16, rows: u16) {
        // A zero-sized terminal would divide by zero all over the layout.
        self.columns.store(columns.max(1), Ordering::Relaxed);
        self.rows.store(rows.max(1), Ordering::Relaxed);
    }

    pub fn get(&self) -> Size {
        Size {
            width: self.columns.load(Ordering::Relaxed),
            height: self.rows.load(Ordering::Relaxed),
        }
    }
}

/// Collects rendered bytes and hands them to the ssh writer task on flush.
pub struct ChannelWriter {
    buffer: Vec<u8>,
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl ChannelWriter {
    pub fn new(tx: mpsc::UnboundedSender<Vec<u8>>) -> Self {
        Self {
            buffer: Vec::with_capacity(8 * 1024),
            tx,
        }
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let frame = std::mem::take(&mut self.buffer);
        self.tx
            .send(frame)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ssh session closed"))
    }
}

pub struct SshBackend {
    inner: CrosstermBackend<ChannelWriter>,
    size: Arc<PtySize>,
    cursor: Position,
}

impl SshBackend {
    pub fn new(writer: ChannelWriter, size: Arc<PtySize>) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            size,
            cursor: Position::ORIGIN,
        }
    }
}

impl Backend for SshBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    /// Tracked, not queried: asking the terminal would block on a reply from
    /// the far end of an ssh connection.
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.cursor = position;
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    /// From the ssh pty request, and updated on every window change.
    fn size(&self) -> io::Result<Size> {
        Ok(self.size.get())
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size.get(),
            // ssh reports pixel dimensions too, but nothing here uses them.
            pixels: Size::default(),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }
}
