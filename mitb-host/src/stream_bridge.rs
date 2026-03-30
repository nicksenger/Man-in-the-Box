use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use wasmtime::component::{
    Destination, Source, StreamConsumer, StreamProducer, StreamResult, VecBuffer,
};

pub(crate) struct MpscStreamProducer<T> {
    rx: mpsc::Receiver<T>,
}

impl<T> MpscStreamProducer<T> {
    pub(crate) fn new(rx: mpsc::Receiver<T>) -> Self {
        Self { rx }
    }
}

impl<T, D> StreamProducer<D> for MpscStreamProducer<T>
where
    T: Send + Sync + 'static,
{
    type Item = T;
    type Buffer = VecBuffer<T>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'a, D>,
        mut dst: Destination<'a, Self::Item, Self::Buffer>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let count = dst.remaining(&mut store).unwrap_or(32);
        if count == 0 {
            return Poll::Ready(Ok(StreamResult::Completed));
        }

        let mut buf = Vec::new();
        loop {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(item)) => {
                    buf.push(item);
                    if buf.len() >= count {
                        dst.set_buffer(buf.into());
                        return Poll::Ready(Ok(StreamResult::Completed));
                    }
                }
                Poll::Ready(None) => {
                    if !buf.is_empty() {
                        dst.set_buffer(buf.into());
                    }
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
                Poll::Pending => {
                    if !buf.is_empty() {
                        dst.set_buffer(buf.into());
                        return Poll::Ready(Ok(StreamResult::Completed));
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}

pub(crate) struct MpscStreamConsumer<T> {
    tx: mpsc::Sender<T>,
    pending: std::collections::VecDeque<T>,
}

impl<T> MpscStreamConsumer<T> {
    pub(crate) fn new(tx: mpsc::Sender<T>) -> Self {
        Self {
            tx,
            pending: std::collections::VecDeque::new(),
        }
    }
}

impl<T, D> StreamConsumer<D> for MpscStreamConsumer<T>
where
    T: wasmtime::component::Lift + Send + Sync + Unpin + 'static,
    D: 'static,
{
    type Item = T;

    fn poll_consume(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<D>,
        mut source: Source<'_, Self::Item>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let this = self.get_mut();

        while let Some(item) = this.pending.pop_front() {
            match this.tx.try_send(item) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(item)) => {
                    this.pending.push_front(item);
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
            }
        }

        let remaining = source.remaining(&mut store);
        if remaining > 0 {
            let mut buf = Vec::with_capacity(remaining);
            source.read(&mut store, &mut buf)?;

            let mut iter = buf.into_iter();
            for item in iter.by_ref() {
                match this.tx.try_send(item) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(item)) => {
                        this.pending.push_back(item);
                        this.pending.extend(iter);
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        return Poll::Ready(Ok(StreamResult::Dropped));
                    }
                }
            }
            Poll::Ready(Ok(StreamResult::Completed))
        } else if finish {
            Poll::Ready(Ok(StreamResult::Cancelled))
        } else {
            Poll::Ready(Ok(StreamResult::Completed))
        }
    }
}
