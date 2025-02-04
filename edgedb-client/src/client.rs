#![cfg_attr(not(feature="unstable"), allow(dead_code))]

use std::collections::HashMap;
use std::fmt;
use std::str;
use std::time::Duration;

use async_std::prelude::StreamExt;
use async_std::future::{timeout, pending};
use async_std::io::prelude::WriteExt;
use async_std::io::ReadExt;
use bytes::{Bytes, BytesMut};
use futures_util::io::{ReadHalf, WriteHalf};
use typemap::TypeMap;
use tls_api::TlsStream;

use edgedb_protocol::QueryResult;
use edgedb_protocol::client_message::ClientMessage;
use edgedb_protocol::client_message::{DescribeStatement, DescribeAspect};
use edgedb_protocol::client_message::{Execute, ExecuteScript};
use edgedb_protocol::client_message::{Prepare, IoFormat, Cardinality};
use edgedb_protocol::descriptors::OutputTypedesc;
use edgedb_protocol::encoding::Output;
use edgedb_protocol::features::ProtocolVersion;
use edgedb_protocol::query_arg::{QueryArgs, Encoder};
use edgedb_protocol::queryable::{Queryable};
use edgedb_protocol::server_message::ServerMessage;
use edgedb_protocol::server_message::{TransactionState};

use crate::debug::PartialDebug;
use crate::errors::{ClientConnectionError, ProtocolError};
use crate::errors::{ClientConnectionTimeoutError, ClientConnectionEosError};
use crate::errors::{ClientInconsistentError, ClientEncodingError};
use crate::errors::{Error, ErrorKind, ResultExt};
use crate::errors::{NoResultExpected, NoDataError};
use crate::errors::{ProtocolOutOfOrderError, ProtocolEncodingError};
use crate::reader::{self, QueryResponse, Reader};
use crate::server_params::ServerParam;


#[derive(Debug)]
/// A single connection to the EdgeDB
pub struct Connection {
    pub(crate) input: ReadHalf<TlsStream>,
    pub(crate) output: WriteHalf<TlsStream>,
    pub(crate) input_buf: BytesMut,
    pub(crate) output_buf: BytesMut,
    pub(crate) version: ProtocolVersion,
    pub(crate) params: TypeMap<dyn typemap::DebugAny + Send + Sync>,
    pub(crate) transaction_state: TransactionState,
    pub(crate) dirty: bool,
}

pub struct Sequence<'a> {
    pub writer: Writer<'a>,
    pub reader: Reader<'a>,
    pub(crate) active: bool,
    pub(crate) dirty: &'a mut bool,
}

pub struct Writer<'a> {
    stream: &'a mut WriteHalf<TlsStream>,
    proto: &'a ProtocolVersion,
    outbuf: &'a mut BytesMut,
}

#[derive(Debug, Clone)]
pub struct StatementParams {
    pub io_format: IoFormat,
    pub cardinality: Cardinality,
}

impl StatementParams {
    pub fn new() -> StatementParams {
        StatementParams {
            io_format: IoFormat::Binary,
            cardinality: Cardinality::Many,
        }
    }
    pub fn io_format(&mut self, fmt: IoFormat) -> &mut Self {
        self.io_format = fmt;
        self
    }
    pub fn cardinality(&mut self, card: Cardinality) -> &mut Self {
        self.cardinality = card;
        self
    }
}


impl<'a> Sequence<'a> {

    pub fn response<T: QueryResult>(self, state: T::State)
        -> QueryResponse<'a, T>
    {
        assert!(self.active);  // TODO(tailhook) maybe debug_assert
        reader::QueryResponse {
            seq: self,
            buffer: Vec::new(),
            error: None,
            complete: false,
            state,
        }
    }

    pub async fn response_blobs(mut self) -> Result<(Vec<Bytes>, Bytes), Error>
    {
        assert!(self.active);  // TODO(tailhook) maybe debug_assert
        let mut data = Vec::new();
        let complete = loop {
            match self.reader.message().await? {
                ServerMessage::Data(m) => data.extend(m.data),
                ServerMessage::CommandComplete(m) => break Ok(m.status_data),
                ServerMessage::ErrorResponse(e) => break Err(e),
                msg => {
                    return Err(ProtocolOutOfOrderError::with_message(format!(
                        "unsolicited packet: {}", PartialDebug(msg))));
                }
            }
        };
        match self.reader.message().await? {
            ServerMessage::ReadyForCommand(r) => {
                self.reader.consume_ready(r);
                self.end_clean();
                return complete.map(|m| (data, m)).map_err(|e| e.into());
            }
            msg => {
                return Err(ProtocolOutOfOrderError::with_message(format!(
                    "unsolicited packet: {}", PartialDebug(msg))));
            }
        }
    }

    pub fn end_clean(&mut self) {
        self.active = false;
        *self.dirty = false;
    }
}

impl Connection {
    pub fn protocol(&self) -> &ProtocolVersion {
        return &self.version
    }
    pub async fn passive_wait<T>(&mut self) -> T {
        let mut buf = [0u8; 1];
        self.input.read(&mut buf[..]).await.ok();
        // any erroneous or successful read (even 0) means need reconnect
        self.dirty = true;
        pending::<()>().await;
        unreachable!();
    }
    pub fn is_consistent(&self) -> bool {
        !self.dirty
    }
    pub async fn terminate(mut self) -> Result<(), Error> {
        let mut seq = self.start_sequence().await?;
        seq.send_messages(&[ClientMessage::Terminate]).await?;
        match seq.message().await {
            Err(e) if e.is::<ClientConnectionEosError>() => Ok(()),
            Err(e) => Err(e),
            Ok(msg) => Err(ProtocolError::with_message(format!(
                "unsolicited message {:?}", msg))),
        }
    }
    pub async fn start_sequence<'x>(&'x mut self)
        -> Result<Sequence<'x>, Error>
    {
        if self.dirty {
            return Err(ClientInconsistentError::with_message(
                "Connection is inconsistent state. Please reconnect."));
        }
        self.dirty = true;
        let reader = Reader {
            proto: &self.version,
            buf: &mut self.input_buf,
            stream: &mut self.input,
            transaction_state: &mut self.transaction_state,
        };
        let writer = Writer {
            proto: &self.version,
            outbuf: &mut self.output_buf,
            stream: &mut self.output,
        };
        Ok(Sequence {
            writer,
            reader,
            active: true,
            dirty: &mut self.dirty,
        })
    }

    pub fn get_param<T: ServerParam>(&self)
        -> Option<&<T as typemap::Key>::Value>
        where <T as typemap::Key>::Value: fmt::Debug + Send + Sync
    {
        self.params.get::<T>()
    }
    pub fn transaction_state(&self) -> TransactionState {
        self.transaction_state
    }
}

impl<'a> Writer<'a> {

    pub async fn send_messages<'x, I>(&mut self, msgs: I) -> Result<(), Error>
        where I: IntoIterator<Item=&'x ClientMessage>
    {
        self.outbuf.truncate(0);
        for msg in msgs {
            msg.encode(&mut Output::new(
                &self.proto,
                self.outbuf,
            )).map_err(ClientEncodingError::with_source)?;
        }
        self.stream.write_all(&self.outbuf[..]).await
            .map_err(ClientConnectionError::with_source)?;
        Ok(())
    }

}


impl<'a> Sequence<'a> {
    pub async fn send_messages<'x, I>(&mut self, msgs: I)
        -> Result<(), Error>
        where I: IntoIterator<Item=&'x ClientMessage>
    {
        assert!(self.active);  // TODO(tailhook) maybe debug_assert
        self.writer.send_messages(msgs).await
    }

    pub async fn expect_ready(&mut self) -> Result<(), Error> {
        assert!(self.active);  // TODO(tailhook) maybe debug_assert
        self.reader.wait_ready().await?;
        self.end_clean();
        Ok(())
    }

    pub fn message(&mut self) -> reader::MessageFuture<'_, 'a> {
        assert!(self.active);  // TODO(tailhook) maybe debug_assert
        self.reader.message()
    }

    // TODO(tailhook) figure out if this is the best way
    pub async fn err_sync(&mut self) -> Result<(), Error> {
        assert!(self.active);  // TODO(tailhook) maybe debug_assert
        self.writer.send_messages(&[ClientMessage::Sync]).await?;
        timeout(Duration::from_secs(10), self.expect_ready()).await
            .map_err(ClientConnectionTimeoutError::with_source)??;
        Ok(())
    }

    pub async fn _process_exec(&mut self) -> Result<Bytes, Error> {
        assert!(self.active);  // TODO(tailhook) maybe debug_assert
        let status = loop {
            let msg = self.reader.message().await?;
            match msg {
                ServerMessage::CommandComplete(c) => {
                    self.reader.wait_ready().await?;
                    self.end_clean();
                    break c.status_data;
                }
                ServerMessage::ErrorResponse(err) => {
                    self.reader.wait_ready().await?;
                    self.end_clean();
                    return Err(err.into());
                }
                ServerMessage::Data(_) => { }
                msg => {
                    eprintln!("WARNING: unsolicited message {:?}", msg);
                }
            }
        };
        Ok(status)
    }

    pub(crate) async fn _query<A>(&mut self, request: &str, arguments: &A,
        bld: &StatementParams)
        -> Result<OutputTypedesc, Error>
        where A: QueryArgs + ?Sized,
    {
        assert!(self.active);  // TODO(tailhook) maybe debug_assert
        let statement_name = Bytes::from_static(b"");

        self.send_messages(&[
            ClientMessage::Prepare(Prepare {
                headers: HashMap::new(),
                io_format: bld.io_format,
                expected_cardinality: bld.cardinality,
                statement_name: statement_name.clone(),
                command_text: String::from(request),
            }),
            ClientMessage::Flush,
        ]).await?;

        loop {
            let msg = self.reader.message().await?;
            match msg {
                ServerMessage::PrepareComplete(..) => {
                    break;
                }
                ServerMessage::ErrorResponse(err) => {
                    self.err_sync().await?;
                    return Err(err.into());
                }
                _ => {
                    return Err(ProtocolOutOfOrderError::with_message(format!(
                        "Unsolicited message {:?}", msg)));
                }
            }
        }

        self.send_messages(&[
            ClientMessage::DescribeStatement(DescribeStatement {
                headers: HashMap::new(),
                aspect: DescribeAspect::DataDescription,
                statement_name: statement_name.clone(),
            }),
            ClientMessage::Flush,
        ]).await?;

        let data_description = loop {
            let msg = self.reader.message().await?;
            match msg {
                ServerMessage::CommandDataDescription(data_desc) => {
                    break data_desc;
                }
                ServerMessage::ErrorResponse(err) => {
                    self.err_sync().await?;
                    return Err(err.into());
                }
                _ => {
                    return Err(ProtocolOutOfOrderError::with_message(format!(
                        "Unsolicited message {:?}", msg)));
                }
            }
        };
        let desc = data_description.output()
            .map_err(ProtocolEncodingError::with_source)?;
        let inp_desc = data_description.input()
            .map_err(ProtocolEncodingError::with_source)?;

        let mut arg_buf = BytesMut::with_capacity(8);
        arguments.encode(&mut Encoder::new(
            &inp_desc.as_query_arg_context(),
            &mut arg_buf,
        ))?;

        self.send_messages(&[
            ClientMessage::Execute(Execute {
                headers: HashMap::new(),
                statement_name: statement_name.clone(),
                arguments: arg_buf.freeze(),
            }),
            ClientMessage::Sync,
        ]).await?;
        Ok(desc)
    }
}

impl Connection {
    pub async fn execute<S>(&mut self, request: S)
        -> Result<Bytes, Error>
        where S: ToString,
    {
        let mut seq = self.start_sequence().await?;
        seq.send_messages(&[
            ClientMessage::ExecuteScript(ExecuteScript {
                headers: HashMap::new(),
                script_text: request.to_string(),
            }),
        ]).await?;
        let status = loop {
            match seq.message().await? {
                ServerMessage::CommandComplete(c) => {
                    seq.expect_ready().await?;
                    break c.status_data;
                }
                ServerMessage::ErrorResponse(err) => {
                    seq.expect_ready().await?;
                    return Err(err.into());
                }
                msg => {
                    eprintln!("WARNING: unsolicited message {:?}", msg);
                }
            }
        };
        Ok(status)
    }

    pub async fn query<R, A>(&mut self, request: &str, arguments: &A)
        -> Result<QueryResponse<'_, R>, Error>
        where R: QueryResult,
              A: QueryArgs,
    {
        let mut seq = self.start_sequence().await?;
        let desc = seq._query(
            request, arguments,
            &StatementParams::new(),
        ).await?;
        match desc.root_pos() {
            Some(root_pos) => {
                let ctx = desc.as_queryable_context();
                let state = R::prepare(&ctx, root_pos)?;
                Ok(seq.response(state))
            }
            None => {
                let completion_message = seq._process_exec().await?;
                Err(NoResultExpected::with_message(
                    String::from_utf8_lossy(&completion_message[..])
                    .to_string()))?
            }
        }
    }

    pub async fn query_row<R, A>(&mut self, request: &str, arguments: &A)
        -> Result<R, Error>
        where R: Queryable,
              A: QueryArgs,
    {
        let mut query = self.query(request, arguments).await?;
        if let Some(result) = query.next().await.transpose()? {
            if let Some(_) = query.next().await.transpose()? {
                query.skip_remaining().await?;
                return Err(ProtocolError::with_message(
                    "extra row returned for query_row"
                ));
            }
            Ok(result)
        } else {
            return Err(NoDataError::build());
        }
    }

    pub async fn query_row_opt<R, A>(&mut self, request: &str, arguments: &A)
        -> Result<Option<R>, Error>
        where R: Queryable,
              A: QueryArgs,
    {
        let mut query = self.query(request, arguments).await?;
        if let Some(result) = query.next().await.transpose()? {
            if let Some(_) = query.next().await.transpose()? {
                return Err(ProtocolError::with_message(
                    "extra row returned for query_row"
                ));
            }
            Ok(Some(result))
        } else {
            Ok(None)
        }
    }

    pub async fn query_json<A>(&mut self, request: &str, arguments: &A)
        -> Result<QueryResponse<'_, String>, Error>
        where A: QueryArgs,
    {
        let mut seq = self.start_sequence().await?;
        let desc = seq._query(
            request, arguments,
            &StatementParams::new().io_format(IoFormat::Json),
        ).await?;
        match desc.root_pos() {
            Some(root_pos) => {
                let ctx = desc.as_queryable_context();
                let state = String::prepare(&ctx, root_pos)?;
                Ok(seq.response(state))
            }
            None => {
                let completion_message = seq._process_exec().await?;
                Err(NoResultExpected::with_message(
                    String::from_utf8_lossy(&completion_message[..])
                    .to_string()))?
            }
        }
    }

    pub async fn query_json_els<A>(&mut self, request: &str, arguments: &A)
        -> Result<QueryResponse<'_, String>, Error>
        where A: QueryArgs,
    {
        let mut seq = self.start_sequence().await?;
        let desc = seq._query(
            request, arguments,
            &StatementParams::new().io_format(IoFormat::JsonElements),
        ).await?;
        match desc.root_pos() {
            Some(root_pos) => {
                let ctx = desc.as_queryable_context();
                let state = String::prepare(&ctx, root_pos)?;
                Ok(seq.response(state))
            }
            None => {
                let completion_message = seq._process_exec().await?;
                Err(NoResultExpected::with_message(
                    String::from_utf8_lossy(&completion_message[..])
                    .to_string()))?
            }
        }
    }

    #[allow(dead_code)]
    pub async fn execute_args<A>(&mut self, request: &str, arguments: &A)
        -> Result<Bytes, Error>
        where A: QueryArgs,
    {
        let mut seq = self.start_sequence().await?;
        seq._query(request, arguments, &StatementParams::new()).await?;
        return seq._process_exec().await;
    }

    pub async fn get_version(&mut self) -> Result<String, Error> {
        self.query_row("SELECT sys::get_version_as_str()", &()).await
        .context("cannot fetch database version")
    }
}


