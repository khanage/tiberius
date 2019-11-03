//! A pure-rust TDS implementation for Microsoft SQL Server (>=2008)
#![allow(unused_imports, dead_code)] // TODO
#![recursion_limit = "256"]

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::mem;
use std::pin::Pin;
use std::result;
use std::sync::{atomic, Arc, Mutex};
use std::task::{self, Poll};
use std::thread;

use futures_util::future::{self, FutureExt};
use futures_util::ready;
use futures_util::stream::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{self, mpsc};
use tracing::{self, debug_span, event, trace_span, Level};

mod collation;
mod connect;
mod error;
mod prepared;
mod protocol;
use protocol::rpc::{
    RpcOptionFlags, RpcParam, RpcProcId, RpcProcIdValue, RpcStatusFlags, TokenRpcRequest,
};
use protocol::ColumnData;
pub use protocol::EncryptionLevel;
mod row;
mod tls;

pub use connect::{connect, connect_tcp, connect_tcp_sql_browser, ConnectParams};
pub use error::Error;
pub type Result<T> = result::Result<T, Error>;
pub use row::Row;

pub(crate) fn get_driver_version() -> u64 {
    env!("CARGO_PKG_VERSION")
        .splitn(6, '.')
        .enumerate()
        .fold(0u64, |acc, part| {
            acc | (part.1.parse::<u64>().unwrap() << (part.0 * 8))
        })
}

#[derive(Debug)]
enum ReceivedToken {
    Row(protocol::TokenRow),
    Done(protocol::TokenDone),
    DoneProc(protocol::TokenDone),
    ReturnStatus(u32),
    ReturnValue(protocol::TokenReturnValue),
}

impl Connection {
    async fn check_pending_unprepares(&mut self) -> Result<()> {
        // Free some handles, if they exist. It's not critical if this fails for whatever reason.
        let mut free_handles: Vec<i32> = vec![];
        if let Ok(ref mut guard) = self.close_handle_queue.lock() {
            // Ensure we save roundtrips
            if guard.len() > 10 {
                mem::swap(&mut free_handles, &mut **guard);
            }
        }
        let (result_sender, _) = mpsc::unbounded_channel();

        for free_handle in free_handles {
            event!(Level::DEBUG, unprepare = free_handle);
            let params = vec![RpcParam {
                name: Cow::Borrowed("handle"),
                flags: RpcStatusFlags::PARAM_BY_REF_VALUE,
                value: ColumnData::I32(free_handle),
            }];

            let req = TokenRpcRequest {
                proc_id: RpcProcIdValue::Id(RpcProcId::SpUnprepare),
                flags: RpcOptionFlags::empty(),
                params,
            };
            let writer = self.writer.clone();
            let mut writer = writer.lock().await;
            self.result_sender
                .try_send(result_sender.clone())
                .expect("TODO");
            req.write_to(&self.ctx, &mut *writer).await?;
        }

        Ok(())
    }

    async fn into_worker_future(
        mut self,
        mut reader: Box<dyn AsyncRead + Unpin>,
        mut result_receiver: mpsc::UnboundedReceiver<mpsc::UnboundedSender<ReceivedToken>>,
    ) -> Result<()> {
        let mut reader = protocol::TokenStreamReader::new(protocol::PacketReader::new(&mut reader));

        let mut next_receiver = true;
        let mut current_receiver = None;
        loop {
            event!(Level::TRACE, "reading next token");
            self.check_pending_unprepares().await?;

            let ty = reader.read_token().await?;
            if next_receiver {
                event!(Level::TRACE, "next_receiver");
                next_receiver = false;
                current_receiver = Some(result_receiver.next().await.unwrap());
            }
            let recv_token = match ty {
                protocol::TokenType::ColMetaData => {
                    let meta = reader.read_colmetadata_token(&self.ctx).await?;
                    continue; // TODO
                }
                protocol::TokenType::Row => {
                    let row = reader.read_row_token(&self.ctx).await?;
                    event!(Level::TRACE, sent_row= ?row);
                    ReceivedToken::Row(row)
                }
                protocol::TokenType::Done | protocol::TokenType::DoneInProc => {
                    let done = reader.read_done_token(&self.ctx).await?;

                    // TODO: make sure we panic when executing 2 queries but only expecting one result
                    if ty == protocol::TokenType::Done
                        && !done.status.contains(protocol::DoneStatus::MORE)
                    {
                        next_receiver = true;
                    }
                    println!("xx: {:?}", ty);
                    ReceivedToken::Done(done)
                }
                protocol::TokenType::DoneProc => {
                    let done = reader.read_done_token(&self.ctx).await?;
                    next_receiver = true;
                    ReceivedToken::DoneProc(done)
                }
                protocol::TokenType::ReturnStatus => {
                    let return_status = reader.read_return_status_token(&self.ctx).await?;
                    ReceivedToken::ReturnStatus(return_status)
                }
                protocol::TokenType::ReturnValue => {
                    let return_value = reader.read_return_value_token(&self.ctx).await?;
                    ReceivedToken::ReturnValue(return_value)
                }
                protocol::TokenType::Error => {
                    let err = reader.read_error_token(&self.ctx).await?;
                    return Err(error::Error::Server(err));
                }
                _ => panic!("Token {:?} unimplemented!", ty),
            };
            event!(Level::TRACE, "recv token: {:?}", recv_token);
            let _ = current_receiver.as_mut().unwrap().try_send(recv_token);
        }
    }
}

/// A connection to a SQL server
#[derive(Clone)]
pub struct Connection {
    ctx: Arc<protocol::Context>,
    writer: Arc<sync::Mutex<Box<dyn AsyncWrite + Unpin>>>,
    conn_handler: future::Shared<Pin<Box<dyn Future<Output = Result<()>>>>>,
    result_sender: mpsc::UnboundedSender<mpsc::UnboundedSender<ReceivedToken>>,
    close_handle_queue: Arc<Mutex<Vec<i32>>>,
}

impl Connection {
    /// Execute a simple query and return multiple resultsets which consist of multiple rows.
    ///
    /// # Warning
    /// Do not use this with any user specified input.  
    /// Please resort to prepared statements ([query](Client::query) or [prepare](Client::prepare)) in order to prevent SQL-Injections.  
    pub async fn simple_query(&self, query: &str) -> Result<impl ResultSet<Result<row::Row>>> {
        let span = debug_span!("simple_query", query = query);
        let _enter = span.enter();

        let writer = self.writer.clone();
        let mut writer = writer.lock().await;

        // Subscribe for results
        let (sender, receiver) = mpsc::unbounded_channel();
        self.result_sender.clone().try_send(sender).expect("TODO");

        // Fire a query
        event!(Level::DEBUG, "WRITING simple QUERY");
        let header = protocol::PacketHeader {
            ty: protocol::PacketType::SQLBatch,
            status: protocol::PacketStatus::NormalMessage,
            ..self.ctx.new_header(0)
        };
        let mut wr = protocol::PacketWriter::new(&mut *writer, header);
        protocol::write_trans_descriptor(&mut wr, &self.ctx, 0 /* TODO */).await?;
        for b2 in query.encode_utf16() {
            let bytes = b2.to_le_bytes();
            wr.write_bytes(&self.ctx, &bytes[..]).await?;
        }
        wr.finish(&self.ctx).await?;
        ::std::mem::drop(writer);

        println!("WAITING for results");
        let qs = QueryStream {
            conn_handler: self.conn_handler.clone(),
            results: receiver,
            done: false,
            has_next_resultset: false,
        };
        Ok(qs)
    }

    async fn rpc_perform_query<'a>(
        &'a self,
        proc_id: RpcProcId,
        mut rpc_params: Vec<RpcParam<'static>>,
        params: &'a [&dyn prepared::ToSql],
        stmt_handle: Arc<atomic::AtomicI32>,
    ) -> Result<QueryStream<PreparedStream>> {
        let mut param_str = String::new();
        for (i, param) in params.iter().enumerate() {
            if i > 0 {
                param_str.push(',')
            }
            param_str.push_str(&format!("@P{} ", i + 1));
            let (sql_type, param_data) = param.to_sql();
            param_str.push_str(sql_type);

            rpc_params.push(RpcParam {
                name: Cow::Owned(format!("@P{}", i + 1)),
                flags: RpcStatusFlags::empty(),
                value: param_data,
            });
        }

        if let Some(params) = rpc_params.iter_mut().find(|x| x.name == "params") {
            params.value = ColumnData::String(param_str.into());
        }

        let req = TokenRpcRequest {
            proc_id: RpcProcIdValue::Id(proc_id),
            flags: RpcOptionFlags::empty(),
            params: rpc_params,
        };

        let writer = self.writer.clone();
        let mut writer = writer.lock().await;

        // Subscribe for results
        let (sender, receiver) = mpsc::unbounded_channel();
        self.result_sender.clone().try_send(sender).expect("TODO");

        // Fire a query
        event!(Level::DEBUG, "QUERY ({:?})", proc_id);

        req.write_to(&self.ctx, &mut *writer).await?;
        ::std::mem::drop(writer);

        println!("WAITING for results");
        let qs = QueryStream {
            conn_handler: self.conn_handler.clone(),
            results: PreparedStream {
                results: receiver,
                stmt_handle,
                read_ahead: None,
            },
            done: false,
            has_next_resultset: false,
        };
        Ok(qs)
    }

    async fn sp_execute_sql(
        &self,
        query: &str,
        params: &[&dyn prepared::ToSql],
    ) -> Result<QueryStream<PreparedStream>> {
        let mut rpc_params = vec![
            RpcParam {
                name: Cow::Borrowed("stmt"),
                flags: RpcStatusFlags::empty(),
                value: ColumnData::String(query.to_owned()),
            },
            RpcParam {
                name: Cow::Borrowed("params"),
                flags: RpcStatusFlags::empty(),
                value: ColumnData::I32(0),
            },
        ];

        let dummy = Arc::new(atomic::AtomicI32::new(0));
        self.rpc_perform_query(RpcProcId::SpExecuteSQL, rpc_params, params, dummy)
            .await
    }

    async fn sp_prep_exec(
        &self,
        ret_handle: Arc<atomic::AtomicI32>,
        query: &str,
        params: &[&dyn prepared::ToSql],
    ) -> Result<QueryStream<PreparedStream>> {
        let mut rpc_params = vec![
            RpcParam {
                name: Cow::Borrowed("handle"),
                flags: RpcStatusFlags::PARAM_BY_REF_VALUE,
                value: ColumnData::I32(0),
            },
            RpcParam {
                name: Cow::Borrowed("params"),
                flags: RpcStatusFlags::empty(),
                value: ColumnData::I32(0),
            },
            RpcParam {
                name: Cow::Borrowed("stmt"),
                flags: RpcStatusFlags::empty(),
                value: ColumnData::String(query.to_owned()),
            },
        ];

        self.rpc_perform_query(RpcProcId::SpPrepExec, rpc_params, params, ret_handle)
            .await
    }

    async fn sp_execute(
        &self,
        stmt_handle: Arc<atomic::AtomicI32>,
        query: &str,
        params: &[&dyn prepared::ToSql],
    ) -> Result<QueryStream<PreparedStream>> {
        let mut rpc_params = vec![RpcParam {
            // handle (using "handle" here makes RpcProcId::SpExecute not work and requires RpcProcIdValue::NAME, wtf)
            // not specifying the name is better anyways to reduce overhead on execute
            name: Cow::Borrowed(""),
            flags: RpcStatusFlags::empty(),
            value: ColumnData::I32(stmt_handle.load(atomic::Ordering::SeqCst)),
        }];

        self.rpc_perform_query(RpcProcId::SpExecute, rpc_params, params, stmt_handle)
            .await
    }

    /// Execute a statement and return each resultset containing rows
    ///
    /// You can access further resultsets using [ResultSet::next_resultset].
    /// # Panics
    /// Panics If you do not handle all resultsets.
    pub async fn query<S>(
        &self,
        stmt: S,
        params: &[&dyn prepared::ToSql],
    ) -> Result<impl ResultSet<Result<row::Row>>>
    where
        S: ToStatement,
    {
        let stmt = stmt.to_stmt();

        match stmt {
            private::StatementRepr::QueryString(ref query) => {
                self.sp_execute_sql(query, params).await
            }
            private::StatementRepr::Statement(ref stmt) => {
                // use sp_executesql for 1st call -> sp_prepexec on 2nd -> then sp_execute
                // as microsoft JDBC driver by default (#83)
                let query = &stmt.query;
                let mut query_signature = format!("{}:{}:", query.len(), query);
                for param in params {
                    query_signature += param.to_sql().0;
                }

                let mut inserted = true;
                let mut stmt_handle = stmt
                    .handles
                    .lock()
                    .expect("TODO")
                    .entry(query_signature)
                    .and_modify(|_| inserted = false)
                    .or_insert_with(|| Arc::new(atomic::AtomicI32::new(0)))
                    .clone();

                if inserted {
                    return self.sp_execute_sql(query, params).await;
                }
                // Now this is already the second call, so we assume (as microsoft JDBCS driver)
                // that more will follow. Actually prepare a statement
                if stmt_handle.load(atomic::Ordering::SeqCst) == 0 {
                    return self.sp_prep_exec(stmt_handle, query, params).await;
                }

                // sp_execute
                self.sp_execute(stmt_handle, query, params).await
            }
        }
    }

    /// Create a statement associated to a given SQL which can be executed later on
    ///
    /// This is a lazy operation and will not do anything until the returned statement is used.
    /// Every statement can contain multiple underlying prepared statements.
    /// Passing differently typed arguments to .query() tells the server to prepare an
    /// additional statement specific for these argument types.
    pub async fn prepare(&self, query: &str) -> Result<Statement> {
        let stmt = Statement {
            query: query.to_owned(),
            handles: Mutex::new(HashMap::new()),
            close_handle_queue: self.close_handle_queue.clone(),
        };
        Ok(stmt)
    }
}

pub struct Statement {
    query: String,
    /// Map from statement SQL type signature to prepared statement ID/handle
    handles: Mutex<HashMap<String, Arc<atomic::AtomicI32>>>,
    close_handle_queue: Arc<Mutex<Vec<i32>>>,
}

impl Drop for Statement {
    fn drop(&mut self) {
        // Mark all handles for cleanup (unprepare)
        if let Ok(handles) = self.handles.get_mut() {
            if let Ok(mut close_queue) = self.close_handle_queue.lock() {
                let cleanable_handles = handles
                    .values()
                    .map(|x| x.load(atomic::Ordering::SeqCst))
                    .filter(|x| *x > 0);
                close_queue.extend(cleanable_handles);
            }
        }
    }
}

mod private {
    use super::Statement;

    pub enum StatementRepr<'a> {
        Statement(&'a Statement),
        QueryString(&'a str),
    }
}

pub trait ToStatement {
    fn to_stmt(&self) -> private::StatementRepr<'_>;
}

impl ToStatement for &str {
    fn to_stmt(&self) -> private::StatementRepr<'_> {
        private::StatementRepr::QueryString(*self)
    }
}

impl ToStatement for &Statement {
    fn to_stmt(&self) -> private::StatementRepr<'_> {
        private::StatementRepr::Statement(self)
    }
}

pub trait ResultSet<I>: tokio::stream::Stream<Item = I> {
    /// Move to the next resultset and make `poll_next` return rows for it
    fn next_resultset(&mut self) -> bool;
}

struct QueryStream<S> {
    conn_handler: future::Shared<Pin<Box<dyn Future<Output = Result<()>>>>>,
    results: S,

    done: bool,
    has_next_resultset: bool,
}

impl<S> ResultSet<Result<row::Row>> for QueryStream<S>
where
    S: tokio::stream::Stream<Item = ReceivedToken> + Unpin,
{
    /// Move to the next resultset and make `poll_next` return rows for it
    fn next_resultset(&mut self) -> bool {
        if self.has_next_resultset {
            self.has_next_resultset = false;
            return true;
        }
        false
    }
}

impl<S> Drop for QueryStream<S> {
    fn drop(&mut self) {
        if !thread::panicking() && self.has_next_resultset {
            panic!("QueryStream dropped but not all resultsets were handled");
        }
    }
}

impl<S> tokio::stream::Stream for QueryStream<S>
where
    S: tokio::stream::Stream<Item = ReceivedToken> + Unpin,
{
    type Item = Result<row::Row>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.done || self.has_next_resultset {
            return Poll::Ready(None);
        }

        // Handle incoming results and paralelly allow the connection
        // to dispatch results to other streams (or us)
        match self.results.poll_next_unpin(cx) {
            Poll::Pending => match self.conn_handler.poll_unpin(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(err)) => Poll::Ready(Some(Err(err))),
                // The connection future never terminates, except for errors.
                Poll::Ready(Ok(_)) => unreachable!(),
            },
            Poll::Ready(Some(token)) => match token {
                ReceivedToken::Row(row) => Poll::Ready(Some(Ok(row::Row(row)))),
                ReceivedToken::Done(ref done) => {
                    self.has_next_resultset = done.status.contains(protocol::DoneStatus::MORE);
                    if !self.has_next_resultset {
                        self.done = true;
                    }
                    Poll::Ready(None)
                }
                ReceivedToken::DoneProc(_)
                | ReceivedToken::ReturnStatus(_)
                | ReceivedToken::ReturnValue(_) => unimplemented!(),
            },
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
        }
    }
}

struct PreparedStream {
    results: mpsc::UnboundedReceiver<ReceivedToken>,
    read_ahead: Option<ReceivedToken>,
    stmt_handle: Arc<atomic::AtomicI32>,
}

impl tokio::stream::Stream for PreparedStream {
    type Item = ReceivedToken;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            if let Some(ReceivedToken::Row(_)) = self.read_ahead {
                return Poll::Ready(self.read_ahead.take());
            }

            let item = ready!(self.results.poll_next_unpin(cx));
            let done_pending = match self.read_ahead {
                Some(ReceivedToken::Done(_)) => true,
                _ => false,
            };
            return match item {
                Some(row_token @ ReceivedToken::Row(_)) if done_pending => {
                    if let Some(read_ahead) = self.read_ahead.take() {
                        self.read_ahead = Some(row_token);
                        return Poll::Ready(Some(read_ahead));
                    }
                    Poll::Ready(Some(row_token))
                }
                Some(ReceivedToken::Done(done))
                    if done.status.contains(protocol::DoneStatus::MORE) =>
                {
                    let pending = self.read_ahead.take();
                    // we do not know yet, if what follows is the trailer of the stored procedure call or another resultset
                    self.read_ahead = Some(ReceivedToken::Done(done));

                    // e.g. if empty resultset
                    if let Some(read_ahead) = pending {
                        return Poll::Ready(Some(read_ahead));
                    }
                    continue;
                }
                Some(ReceivedToken::DoneProc(done)) => {
                    // ... other stored procedures that we "called"
                    if done.status.contains(protocol::DoneStatus::MORE) {
                        continue;
                    }
                    // signal completion of all resultsets, when the stored procedure completed
                    Poll::Ready(Some(ReceivedToken::Done(done)))
                }
                // TODO: ensure it's the "last" one
                Some(ReceivedToken::ReturnValue(ref ret_val)) => {
                    let handle = match ret_val.value {
                        ColumnData::I32(handle) => handle,
                        _ => unreachable!(),
                    };
                    // TODO: think about multiple competing prepares (=> we prepared the same thing multiple times,
                    //       because stmt handle not ready yet and 0 is still stored before updated by a successful prepare)
                    self.stmt_handle.store(handle, atomic::Ordering::SeqCst); // TODO
                    continue;
                }
                Some(ReceivedToken::ReturnStatus(_)) => continue,
                item => Poll::Ready(item),
            };
        }
    }
}
