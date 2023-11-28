use super::ConnectionLike;
use crate::aio::setup_connection;
use crate::cmd::Cmd;
use crate::connection::RedisConnectionInfo;
#[cfg(any(feature = "tokio-comp", feature = "async-std-comp"))]
use crate::parser::ValueCodec;
use crate::types::{RedisError, RedisFuture, RedisResult, Value};
use ::tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot},
};
use futures_util::{
    future::{Future, FutureExt},
    ready,
    sink::Sink,
    stream::{self, Stream, StreamExt, TryStreamExt as _},
};
use pin_project_lite::pin_project;
use std::collections::VecDeque;
use std::fmt;
use std::fmt::Debug;
use std::io;
use std::pin::Pin;
use std::task::{self, Poll};
#[cfg(any(feature = "tokio-comp", feature = "async-std-comp"))]
use tokio_util::codec::Decoder;

// Senders which the result of a single request are sent through
type PipelineOutput = oneshot::Sender<RedisResult<Value>>;

struct InFlight {
    output: PipelineOutput,
    expected_response_count: usize,
    current_response_count: usize,
    buffer: Option<Value>,
    first_err: Option<RedisError>,
}

impl InFlight {
    fn new(output: PipelineOutput, expected_response_count: usize) -> Self {
        Self {
            output,
            expected_response_count,
            current_response_count: 0,
            buffer: None,
            first_err: None,
        }
    }
}

// A single message sent through the pipeline
struct PipelineMessage<S> {
    input: S,
    output: PipelineOutput,
    response_count: usize,
}

/// Wrapper around a `Stream + Sink` where each item sent through the `Sink` results in one or more
/// items being output by the `Stream` (the number is specified at time of sending). With the
/// interface provided by `Pipeline` an easy interface of request to response, hiding the `Stream`
/// and `Sink`.
struct Pipeline<SinkItem>(mpsc::Sender<PipelineMessage<SinkItem>>);

impl<SinkItem> Clone for Pipeline<SinkItem> {
    fn clone(&self) -> Self {
        Pipeline(self.0.clone())
    }
}

impl<SinkItem> Debug for Pipeline<SinkItem>
where
    SinkItem: Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Pipeline").field(&self.0).finish()
    }
}

pin_project! {
    struct PipelineSink<T> {
        #[pin]
        sink_stream: T,
        in_flight: VecDeque<InFlight>,
        error: Option<RedisError>,
    }
}

impl<T> PipelineSink<T>
where
    T: Stream<Item = RedisResult<Value>> + 'static,
{
    fn new<SinkItem>(sink_stream: T) -> Self
    where
        T: Sink<SinkItem, Error = RedisError> + Stream<Item = RedisResult<Value>> + 'static,
    {
        PipelineSink {
            sink_stream,
            in_flight: VecDeque::new(),
            error: None,
        }
    }

    // Read messages from the stream and send them back to the caller
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<Result<(), ()>> {
        loop {
            // No need to try reading a message if there is no message in flight
            if self.in_flight.is_empty() {
                return Poll::Ready(Ok(()));
            }
            let item = match ready!(self.as_mut().project().sink_stream.poll_next(cx)) {
                Some(result) => result,
                // The redis response stream is not going to produce any more items so we `Err`
                // to break out of the `forward` combinator and stop handling requests
                None => return Poll::Ready(Err(())),
            };
            self.as_mut().send_result(item);
        }
    }

    fn send_result(self: Pin<&mut Self>, result: RedisResult<Value>) {
        let self_ = self.project();

        {
            let entry = match self_.in_flight.front_mut() {
                Some(entry) => entry,
                None => return,
            };

            match result {
                Ok(item) => {
                    entry.buffer = Some(match entry.buffer.take() {
                        Some(Value::Array(mut values)) => {
                            values.push(item);
                            Value::Array(values)
                        }
                        Some(value) => Value::Array(vec![value, item]),
                        None => item,
                    });
                }
                Err(err) => {
                    if entry.first_err.is_none() {
                        entry.first_err = Some(err);
                    }
                }
            }

            entry.current_response_count += 1;
            if entry.current_response_count < entry.expected_response_count {
                // Need to gather more response values
                return;
            }
        }

        let entry = self_.in_flight.pop_front().unwrap();
        let response = match entry.first_err {
            Some(err) => Err(err),
            None => Ok(entry.buffer.unwrap_or(Value::Array(vec![]))),
        };

        // `Err` means that the receiver was dropped in which case it does not
        // care about the output and we can continue by just dropping the value
        // and sender
        entry.output.send(response).ok();
    }
}

impl<SinkItem, T> Sink<PipelineMessage<SinkItem>> for PipelineSink<T>
where
    T: Sink<SinkItem, Error = RedisError> + Stream<Item = RedisResult<Value>> + 'static,
{
    type Error = ();

    // Retrieve incoming messages and write them to the sink
    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context,
    ) -> Poll<Result<(), Self::Error>> {
        match ready!(self.as_mut().project().sink_stream.poll_ready(cx)) {
            Ok(()) => Ok(()).into(),
            Err(err) => {
                *self.project().error = Some(err);
                Ok(()).into()
            }
        }
    }

    fn start_send(
        mut self: Pin<&mut Self>,
        PipelineMessage {
            input,
            output,
            response_count,
        }: PipelineMessage<SinkItem>,
    ) -> Result<(), Self::Error> {
        // If there is nothing to receive our output we do not need to send the message as it is
        // ambiguous whether the message will be sent anyway. Helps shed some load on the
        // connection.
        if output.is_closed() {
            return Ok(());
        }

        let self_ = self.as_mut().project();

        if let Some(err) = self_.error.take() {
            let _ = output.send(Err(err));
            return Err(());
        }

        match self_.sink_stream.start_send(input) {
            Ok(()) => {
                self_
                    .in_flight
                    .push_back(InFlight::new(output, response_count));
                Ok(())
            }
            Err(err) => {
                let _ = output.send(Err(err));
                Err(())
            }
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context,
    ) -> Poll<Result<(), Self::Error>> {
        ready!(self
            .as_mut()
            .project()
            .sink_stream
            .poll_flush(cx)
            .map_err(|err| {
                self.as_mut().send_result(Err(err));
            }))?;
        self.poll_read(cx)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context,
    ) -> Poll<Result<(), Self::Error>> {
        // No new requests will come in after the first call to `close` but we need to complete any
        // in progress requests before closing
        if !self.in_flight.is_empty() {
            ready!(self.as_mut().poll_flush(cx))?;
        }
        let this = self.as_mut().project();
        this.sink_stream.poll_close(cx).map_err(|err| {
            self.send_result(Err(err));
        })
    }
}

impl<SinkItem> Pipeline<SinkItem>
where
    SinkItem: Send + 'static,
{
    fn new<T>(sink_stream: T) -> (Self, impl Future<Output = ()>)
    where
        T: Sink<SinkItem, Error = RedisError> + Stream<Item = RedisResult<Value>> + 'static,
        T: Send + 'static,
        T::Item: Send,
        T::Error: Send,
        T::Error: ::std::fmt::Debug,
    {
        const BUFFER_SIZE: usize = 50;
        let (sender, mut receiver) = mpsc::channel(BUFFER_SIZE);
        let f = stream::poll_fn(move |cx| receiver.poll_recv(cx))
            .map(Ok)
            .forward(PipelineSink::new::<SinkItem>(sink_stream))
            .map(|_| ());
        (Pipeline(sender), f)
    }

    // `None` means that the stream was out of items causing that poll loop to shut down.
    async fn send_single(&mut self, item: SinkItem) -> Result<Value, Option<RedisError>> {
        self.send_recv(item, 1).await
    }

    async fn send_recv(
        &mut self,
        input: SinkItem,
        count: usize,
    ) -> Result<Value, Option<RedisError>> {
        let (sender, receiver) = oneshot::channel();

        self.0
            .send(PipelineMessage {
                input,
                response_count: count,
                output: sender,
            })
            .await
            .map_err(|_| None)?;
        match receiver.await {
            Ok(result) => result.map_err(Some),
            Err(_) => {
                // The `sender` was dropped which likely means that the stream part
                // failed for one reason or another
                Err(None)
            }
        }
    }
}

/// A connection object which can be cloned, allowing requests to be be sent concurrently
/// on the same underlying connection (tcp/unix socket).
#[derive(Clone)]
pub struct MultiplexedConnection {
    pipeline: Pipeline<Vec<u8>>,
    db: i64,
}

impl Debug for MultiplexedConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MultiplexedConnection")
            .field("pipeline", &self.pipeline)
            .field("db", &self.db)
            .finish()
    }
}

impl MultiplexedConnection {
    /// Constructs a new `MultiplexedConnection` out of a `AsyncRead + AsyncWrite` object
    /// and a `ConnectionInfo`
    pub async fn new<C>(
        connection_info: &RedisConnectionInfo,
        stream: C,
    ) -> RedisResult<(Self, impl Future<Output = ()>)>
    where
        C: Unpin + AsyncRead + AsyncWrite + Send + 'static,
    {
        fn boxed(
            f: impl Future<Output = ()> + Send + 'static,
        ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(f)
        }

        #[cfg(all(not(feature = "tokio-comp"), not(feature = "async-std-comp")))]
        compile_error!("tokio-comp or async-std-comp features required for aio feature");

        let codec = ValueCodec::default()
            .framed(stream)
            .and_then(|msg| async move { msg });
        let (pipeline, driver) = Pipeline::new(codec);
        let driver = boxed(driver);
        let mut con = MultiplexedConnection {
            pipeline,
            db: connection_info.db,
        };
        let driver = {
            let auth = setup_connection(connection_info, &mut con);
            futures_util::pin_mut!(auth);

            match futures_util::future::select(auth, driver).await {
                futures_util::future::Either::Left((result, driver)) => {
                    result?;
                    driver
                }
                futures_util::future::Either::Right(((), _)) => {
                    unreachable!("Multiplexed connection driver unexpectedly terminated")
                }
            }
        };
        Ok((con, driver))
    }

    /// Sends an already encoded (packed) command into the TCP socket and
    /// reads the single response from it.
    pub async fn send_packed_command(&mut self, cmd: &Cmd) -> RedisResult<Value> {
        let value = self
            .pipeline
            .send_single(cmd.get_packed_command())
            .await
            .map_err(|err| {
                err.unwrap_or_else(|| RedisError::from(io::Error::from(io::ErrorKind::BrokenPipe)))
            })?;
        Ok(value)
    }

    /// Sends multiple already encoded (packed) command into the TCP socket
    /// and reads `count` responses from it.  This is used to implement
    /// pipelining.
    pub async fn send_packed_commands(
        &mut self,
        cmd: &crate::Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisResult<Vec<Value>> {
        let value = self
            .pipeline
            .send_recv(cmd.get_packed_pipeline(), offset + count)
            .await
            .map_err(|err| {
                err.unwrap_or_else(|| RedisError::from(io::Error::from(io::ErrorKind::BrokenPipe)))
            })?;

        match value {
            Value::Array(mut values) => {
                values.drain(..offset);
                Ok(values)
            }
            _ => Ok(vec![value]),
        }
    }
}

impl ConnectionLike for MultiplexedConnection {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        (async move { self.send_packed_command(cmd).await }).boxed()
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a crate::Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        (async move { self.send_packed_commands(cmd, offset, count).await }).boxed()
    }

    fn get_db(&self) -> i64 {
        self.db
    }
}
