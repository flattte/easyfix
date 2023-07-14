use std::{
    cell::RefCell,
    collections::{hash_map::Entry, HashMap},
    rc::Rc,
};

use easyfix_messages::messages::{FixtMessage, Message};
use futures_util::{pin_mut, Stream};
use once_cell::unsync::Lazy;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
    time::{timeout, Duration},
};
use tokio_stream::StreamExt;
use tracing::{debug, error, info, info_span, Instrument};

use crate::{
    acceptor::{ActiveSessionsMap, SessionsMap},
    application::{Emitter, FixEventInternal},
    messages_storage::MessagesStorage,
    session::Session,
    session_id::SessionId,
    session_state::State,
    settings::{SessionSettings, Settings},
    DisconnectReason, Error, Sender, SessionError, NO_INBOUND_TIMEOUT_PADDING,
};

mod input_stream;
use input_stream::{input_stream, InputEvent};

mod output_stream;
use output_stream::{output_stream, OutputEvent};

pub struct Disconnect;

// TODO: cfg(mt) on mt build
static mut SENDERS: Lazy<HashMap<SessionId, Sender>> = Lazy::new(HashMap::new);

fn senders() -> &'static HashMap<SessionId, Sender> {
    Lazy::force(unsafe { &SENDERS })
}

fn senders_mut() -> &'static mut HashMap<SessionId, Sender> {
    Lazy::force_mut(unsafe { &mut SENDERS })
}

pub fn register_sender(session_id: SessionId, sender: Sender) {
    if let Entry::Vacant(entry) = senders_mut().entry(session_id) {
        entry.insert(sender);
    }
}

pub fn unregister_sender(session_id: &SessionId) {
    if senders_mut().remove(session_id).is_none() {
        // TODO: ERROR?
    }
}

pub fn sender(session_id: &SessionId) -> Option<&Sender> {
    senders().get(session_id)
}

// TODO: Remove?
pub fn send(session_id: &SessionId, msg: Box<Message>) -> Result<(), Box<Message>> {
    if let Some(sender) = sender(session_id) {
        sender.send(msg);
        Ok(())
    } else {
        Err(msg)
    }
}

pub fn send_raw(msg: Box<FixtMessage>) -> Result<(), Box<FixtMessage>> {
    if let Some(sender) = sender(&SessionId::from_input_msg(&msg)) {
        sender.send_raw(msg);
        Ok(())
    } else {
        Err(msg)
    }
}

async fn first_msg(
    stream: &mut (impl Stream<Item = InputEvent> + Unpin),
    logon_timeout: Duration,
) -> Result<Box<FixtMessage>, Error> {
    match timeout(logon_timeout, stream.next()).await {
        Ok(Some(InputEvent::Message(msg))) => Ok(msg),
        Ok(Some(InputEvent::IoError(error))) => Err(error.into()),
        Ok(Some(InputEvent::DeserializeError(error))) => {
            error!("failed to deserialize first message: {error}");
            Err(Error::SessionError(SessionError::LogonNeverReceived))
        }
        _ => Err(Error::SessionError(SessionError::LogonNeverReceived)),
    }
}

#[derive(Debug)]
pub(crate) struct Connection<S> {
    session: Rc<Session<S>>,
}

pub(crate) async fn acceptor_connection<S>(
    tcp_stream: TcpStream,
    settings: Settings,
    sessions: Rc<RefCell<SessionsMap<S>>>,
    active_sessions: Rc<RefCell<ActiveSessionsMap<S>>>,
    emitter: Emitter,
) -> Result<(), Error>
where
    S: MessagesStorage,
{
    let (source, sink) = tcp_stream.into_split();
    let stream = input_stream(source);
    let logon_timeout =
        settings.auto_disconnect_after_no_logon_received + NO_INBOUND_TIMEOUT_PADDING;
    pin_mut!(stream);
    let msg = first_msg(&mut stream, logon_timeout).await?;
    let session_id = SessionId::from_input_msg(&msg);
    debug!("first_msg: {msg:?}");

    let (sender, receiver) = mpsc::unbounded_channel();
    let sender = Sender::new(sender);

    let (session_settings, session_state) = sessions
        .borrow()
        .get_session(&session_id)
        .ok_or(Error::SessionError(SessionError::UnknownSession))?;
    register_sender(session_id.clone(), sender.clone());
    let session = Rc::new(Session::new(
        settings,
        session_settings,
        session_state,
        sender,
        emitter.clone(),
    ));
    active_sessions
        .borrow_mut()
        .insert(session_id.clone(), session.clone());

    let session_span = info_span!(
        "session",
        id = %session_id
    );

    let input_loop_span = info_span!(parent: &session_span, "in");
    let output_loop_span = info_span!(parent: &session_span, "out");

    session
        .on_message_in(msg)
        .instrument(input_loop_span.clone())
        .await;
    // TODO: Not here!, send this event when SessionState is created!
    emitter
        .send(FixEventInternal::Created(session_id.clone()))
        .await;

    let input_stream = stream
        .timeout(session.heartbeat_interval() + NO_INBOUND_TIMEOUT_PADDING)
        .map(|res| res.unwrap_or(InputEvent::Timeout));
    pin_mut!(input_stream);

    let output_stream = output_stream(session.clone(), session.heartbeat_interval(), receiver);
    pin_mut!(output_stream);

    let connection = Connection::new(session);

    let ret = tokio::try_join!(
        connection
            .input_loop(input_stream)
            .instrument(input_loop_span),
        connection
            .output_loop(sink, output_stream)
            .instrument(output_loop_span),
    );
    info!("connection closed");
    // TODO: error here?
    connection.session.on_disconnect().await;
    unregister_sender(&session_id);
    active_sessions.borrow_mut().remove(&session_id);
    ret.map(|_| ())
}

pub(crate) async fn initiator_connection<S>(
    tcp_stream: TcpStream,
    settings: Settings,
    session_settings: SessionSettings,
    state: Rc<RefCell<State<S>>>,
    active_sessions: Rc<RefCell<ActiveSessionsMap<S>>>,
    emitter: Emitter,
) -> Result<(), Error>
where
    S: MessagesStorage,
{
    let (source, sink) = tcp_stream.into_split();
    let session_id = session_settings.session_id.clone();

    let (sender, receiver) = mpsc::unbounded_channel();
    let sender = Sender::new(sender);

    register_sender(session_id.clone(), sender.clone());
    let session = Rc::new(Session::new(
        settings,
        session_settings,
        state,
        sender,
        emitter.clone(),
    ));
    active_sessions
        .borrow_mut()
        .insert(session_id.clone(), session.clone());

    let session_span = info_span!(
        "session",
        id = %session_id
    );

    let input_loop_span = info_span!(parent: &session_span, "in");
    let output_loop_span = info_span!(parent: &session_span, "out");

    // TODO: Not here!, send this event when SessionState is created!
    emitter
        .send(FixEventInternal::Created(session_id.clone()))
        .await;

    let input_stream = input_stream(source)
        .timeout(session.heartbeat_interval() + NO_INBOUND_TIMEOUT_PADDING)
        .map(|res| res.unwrap_or(InputEvent::Timeout));
    pin_mut!(input_stream);

    let output_stream = output_stream(session.clone(), session.heartbeat_interval(), receiver);
    pin_mut!(output_stream);

    // TODO: It's not so simple, add check if session time is within range,
    //       if not schedule timer to send logon at proper time
    session.send_logon_request(&mut session.state().borrow_mut());

    let connection = Connection::new(session);

    let ret = tokio::try_join!(
        connection
            .input_loop(input_stream)
            .instrument(input_loop_span),
        connection
            .output_loop(sink, output_stream)
            .instrument(output_loop_span),
    );
    info!("connection closed");
    // TODO: error here?
    connection.session.on_disconnect().await;
    unregister_sender(&session_id);
    active_sessions.borrow_mut().remove(&session_id);
    ret.map(|_| ())
}

impl<S: MessagesStorage> Connection<S> {
    pub(crate) fn new(session: Rc<Session<S>>) -> Connection<S> {
        Connection { session }
    }

    async fn input_loop(
        &self,
        mut input_stream: impl Stream<Item = InputEvent> + Unpin,
    ) -> Result<(), Error> {
        while let Some(event) = input_stream.next().await {
            match event {
                InputEvent::Message(msg) => {
                    if let Some(Disconnect) = self.session.on_message_in(msg).await {
                        info!("disconnect, exit input processing");
                        break;
                    }
                }
                InputEvent::DeserializeError(error) => {
                    self.session.on_deserialize_error(error).await
                }
                InputEvent::IoError(error) => return self.session.on_io_error(error).await,
                InputEvent::Timeout => self.session.on_in_timeout().await,
            }
        }
        Ok(())
    }

    async fn output_loop(
        &self,
        mut sink: impl AsyncWrite + Unpin,
        mut output_stream: impl Stream<Item = OutputEvent> + Unpin,
    ) -> Result<(), Error> {
        while let Some(event) = output_stream.next().await {
            match event {
                OutputEvent::Message(msg) => {
                    if let Err(error) = sink.write_all(&msg).await {
                        return self.session.on_io_error(error).await;
                    }
                }
                OutputEvent::Timeout => self.session.on_out_timeout().await,
                OutputEvent::Disconnect(reason) => {
                    if let Err(e) = sink.flush().await {
                        error!("final flush failed: {e}");
                    }
                    // XXX: Emit logout here instead of Session::disconnect,
                    //      so `Logout` event will be delivered after Logout
                    //      message instead of randomly before or after.
                    self.session.emit_logout(reason).await;
                    info!("disconnect, exit output processing");
                    return Ok(());
                }
            }
        }
        self.session
            .emit_logout(DisconnectReason::ConnectionLost)
            .await;
        Ok(())
    }
}
