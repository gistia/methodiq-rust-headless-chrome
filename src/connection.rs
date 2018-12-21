use std::sync::mpsc;

use cdp::{HasCdpCommand, SerializeCdpCommand};
use log::*;
use serde;
use serde::{Deserialize};
use serde::de::DeserializeOwned;
use serde_json::json;
use websocket::{ClientBuilder, OwnedMessage};
use websocket::client::sync::Client;
use websocket::stream::sync::TcpStream;
use websocket::WebSocketError;

use crate::cdtp;
use crate::cdtp::{CallId, Response, EventMessage};

use crate::chrome;
use crate::errors::*;
use crate::waiting_call_registry;

pub struct Connection {
    sender: websocket::sender::Writer<TcpStream>,
    next_call_id: CallId,
    call_registry: waiting_call_registry::WaitingCallRegistry,
}

impl Connection {
    pub fn new(browser_id: &chrome::BrowserId, target_messages_tx: mpsc::Sender<cdtp::Message>) -> Result<Self> {
        let connection = Connection::websocket_connection(&browser_id)?;

        let (websocket_receiver, sender) = connection.split().chain_err(|| "Couldn't split conn")?;

        let (browser_responses_tx, browser_responses_rx) = mpsc::channel();
        let call_registry = waiting_call_registry::WaitingCallRegistry::new(browser_responses_rx);

        let _message_handling_thread = std::thread::spawn(move || {
            info!("starting msg dispatching loop");
            Self::dispatch_incoming_messages(websocket_receiver, target_messages_tx, browser_responses_tx);
            info!("quit loop msg dispatching loop");
        });

        Ok(Connection {
            call_registry,
            sender,
            next_call_id: 0,
        })
    }

    fn dispatch_incoming_messages(mut receiver: websocket::receiver::Reader<TcpStream>,
                                  target_messages_tx: mpsc::Sender<cdtp::Message>,
                                  browser_responses_tx: mpsc::Sender<Response>)
    {
        for ws_message in receiver.incoming_messages() {
            match ws_message {
                Err(error) => {
                    match error {
                        WebSocketError::NoDataAvailable => {}
                        _ => { panic!("Unhandled WebSocket error: {:?}", error) }
                    }
                }
                Ok(message) => {
                    if let OwnedMessage::Text(message_string) = message {
                        trace!("Raw message: {:?}", message_string);
                        let message = cdtp::parse_raw_message(message_string);

                        match message {
                            cdtp::Message::Response(response) => {
                                browser_responses_tx.send(response).expect("failed to send to message to page session");
                            }

                            cdtp::Message::Event(event) => {
                                match event {
                                    EventMessage::ReceivedMessageFromTarget(target_message_event) => {
                                        let target_message = cdtp::parse_raw_message(target_message_event.params.message);
                                        target_messages_tx.send(target_message).expect("failed to send to page session");
                                    }
                                    _ => {
                                        trace!("Browser received event: {:?}", event);
                                    }
                                }
                            }
                        }
                    } else {
                        panic!("Got a weird message: {:?}", message)
                    }
                }
            }
        }
    }

    pub fn websocket_connection(browser_id: &chrome::BrowserId) -> Result<Client<TcpStream>> {
        // TODO: can't keep using that proxy forever, will need to deal with chromes on other ports
        let ws_url = &format!("ws://127.0.0.1:9223/devtools/browser/{}", browser_id);
        info!("Connecting to WebSocket: {}", ws_url);
        let client = ClientBuilder::new(ws_url)
            .chain_err(|| "Unable to create client builder")?
            .connect_insecure()
            .chain_err(|| "Unable to connect to WebSocket")?;

        info!("Successfully connected to WebSocket: {}", ws_url);

        Ok(client)
    }

    pub fn call<C>(&mut self, method: C) -> Result<C::ReturnObject>
        where C: cdtp::Method + serde::Serialize {
        let call = method.to_method_call();
        let message = websocket::Message::text(serde_json::to_string(&call).unwrap());

        self.sender.send_message(&message).unwrap();

        let response_rx = self.call_registry.register_call(call.id);

        let response = response_rx.recv().unwrap();

        let result: C::ReturnObject = serde_json::from_value(response.result).unwrap();

        Ok(result)
    }

    pub fn call_method<'a, R>(&mut self, command: &R::Command) -> Result<R>
        where R: DeserializeOwned + HasCdpCommand<'a>,
              <R as cdp::HasCdpCommand<'a>>::Command: serde::ser::Serialize + SerializeCdpCommand
    {
        trace!("Calling method");

        let call_id = self.next_call_id;
        self.next_call_id += 1;

        let method = json!({"method": command.command_name(), "id": call_id, "params": command});
        trace!("sending message: {:#?}", &method);
        let message = websocket::Message::text(serde_json::to_string(&method).unwrap());

        // what if this fails and the waiting method is left there forever? memory leak...
        self.sender.send_message(&message).unwrap();
        let response_rx = self.call_registry.register_call(call_id);

        let raw_response = response_rx.recv().unwrap();
        trace!("method caller got response");
        let method_response = serde_json::from_value::<R>(raw_response.result).unwrap();
        Ok(method_response as R)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    #[test]
    fn you_can_send_methods() {
        env_logger::try_init().unwrap_or(());
        let chrome = super::chrome::Chrome::new(true).unwrap();

        let (messages_tx, _messages_rx) = mpsc::channel::<crate::cdtp::Message>();

        let mut conn = super::Connection::new(&chrome.browser_id, messages_tx).unwrap();

        let call = super::cdtp::target::methods::CreateBrowserContext {};
        let r1 = conn.call(call);

        dbg!(r1);

//        let _response1 = conn.call_method::<cdp::target::CreateBrowserContextResponse>(&cdp::target::CreateBrowserContextCommand {});
//        let _response2 = conn.call_method::<cdp::target::GetBrowserContextsResponse>(&cdp::target::GetBrowserContextsCommand {}).unwrap();
//        let response3 = conn.call_method::<cdp::target::GetTargetsResponse>(&cdp::target::GetTargetsCommand {}).unwrap();
//        let first_target = &response3.target_infos[0];

//        assert_eq!("about:blank", first_target.url);
    }
}
