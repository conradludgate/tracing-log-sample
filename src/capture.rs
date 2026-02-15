use std::cell::{RefCell, RefMut};
use std::io;

use thread_local::ThreadLocal;
use tracing_subscriber::fmt::MakeWriter;

pub(crate) type CaptureBuffer = ThreadLocal<RefCell<Vec<u8>>>;

pub(crate) fn take_captured(buf: &CaptureBuffer) -> Vec<u8> {
    buf.get_or_default().take()
}

pub(crate) fn return_captured(buf: &CaptureBuffer, mut v: Vec<u8>) {
    v.clear();
    buf.get_or_default().replace(v);
}

pub(crate) struct CaptureWriter<'a> {
    buf: RefMut<'a, Vec<u8>>,
}

impl io::Write for CaptureWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct CaptureMakeWriter(pub(crate) CaptureBuffer);

impl<'a> MakeWriter<'a> for CaptureMakeWriter {
    type Writer = CaptureWriter<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter {
            buf: self.0.get_or_default().borrow_mut(),
        }
    }
}
