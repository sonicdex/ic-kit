use crate::call::{CallBuilder, CallReply};
use crate::canister::Canister;
use crate::types::*;
use ic_kit_sys::types::RejectionCode;
use ic_types::Principal;
use std::collections::HashMap;
use std::future::Future;
use std::panic::{RefUnwindSafe, UnwindSafe};
use tokio::sync::{mpsc, oneshot};

/// A local replica that contains one or several canisters.
pub struct Replica {
    sender: mpsc::UnboundedSender<ReplicaMessage>,
}

pub struct CanisterHandle<'a> {
    replica: &'a Replica,
    canister_id: Principal,
}

/// A message we want to send to a canister.
struct CanisterMessage {
    message: Message,
    reply_sender: Option<oneshot::Sender<CallReply>>,
}

enum ReplicaMessage {
    CanisterAdded {
        canister_id: Principal,
        channel: mpsc::UnboundedSender<CanisterMessage>,
    },
    CanisterRequest {
        canister_id: Principal,
        message: Message,
        reply_sender: Option<oneshot::Sender<CallReply>>,
    },
    CanisterReply {
        canister_id: Principal,
        message: Message,
    },
}

impl Replica {
    /// Create a new replica with the given canister.
    pub fn new(canisters: Vec<Canister>) -> Self {
        let tmp = Replica::default();

        for canister in canisters {
            tmp.add_canister(canister);
        }

        tmp
    }

    /// Add the given canister to this replica.
    pub fn add_canister(&self, canister: Canister) -> CanisterHandle {
        let canister_id = canister.id();

        // Create a execution queue for the canister so we can send messages to the canister
        // asynchronously
        let replica_sender = self.sender.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        replica_sender
            .send(ReplicaMessage::CanisterAdded {
                canister_id,
                channel: tx,
            })
            .unwrap_or_else(|_| panic!("ic-kit-runtime: could not send message to replica"));

        // Start the event loop for the canister.
        tokio::spawn(async move {
            let canister_id = canister.id();
            let mut rx = rx;
            let mut canister = canister;

            while let Some(message) = rx.recv().await {
                let perform_call = canister
                    .process_message(message.message, message.reply_sender)
                    .await;

                for call in perform_call {
                    let request_id = call.request_id;
                    let (tx, rx) = oneshot::channel();

                    replica_sender
                        .send(ReplicaMessage::CanisterRequest {
                            canister_id: call.callee,
                            message: call.into(),
                            reply_sender: Some(tx),
                        })
                        .unwrap_or_else(|_| {
                            panic!("ic-kit-runtime: could not send message to replica")
                        });

                    let rs = replica_sender.clone();
                    tokio::spawn(async move {
                        let replica_sender = rs;

                        // wait for the response from the destination canister.
                        let response = rx.await.expect(
                            "ic-kit-runtime: Could not get the response of inter-canister call.",
                        );

                        let message = response.to_message(request_id);

                        replica_sender
                            .send(ReplicaMessage::CanisterReply {
                                canister_id,
                                message,
                            })
                            .unwrap_or_else(|_| {
                                panic!("ic-kit-runtime: could not send message to replica")
                            });
                    });
                }
            }
        });

        CanisterHandle {
            replica: self,
            canister_id,
        }
    }

    /// Return the handle to a canister.
    pub fn get_canister(&self, canister_id: Principal) -> CanisterHandle {
        CanisterHandle {
            replica: &self,
            canister_id,
        }
    }

    /// Enqueue the given request to the destination canister.
    pub(crate) fn enqueue_request(
        &self,
        canister_id: Principal,
        message: Message,
        reply_sender: Option<oneshot::Sender<CallReply>>,
    ) {
        self.sender
            .send(ReplicaMessage::CanisterRequest {
                canister_id,
                message,
                reply_sender,
            })
            .unwrap_or_else(|_| panic!("ic-kit-runtime: could not send message to replica"));
    }

    /// Perform the given call in this replica and return a future that will be resolved once the
    /// call is executed.
    pub(crate) fn perform_call(&self, call: CanisterCall) -> impl Future<Output = CallReply> {
        let canister_id = call.callee;
        let message = Message::from(call);
        let (tx, rx) = oneshot::channel();
        self.enqueue_request(canister_id, message, Some(tx));
        async {
            rx.await
                .expect("ic-kit-runtime: Could not retrieve the response from the call.")
        }
    }

    /// Create a new call builder on the replica, that can be used to send a request to the given
    /// canister.
    pub fn new_call<S: Into<String>>(&self, id: Principal, method: S) -> CallBuilder {
        CallBuilder::new(&self, id, method.into())
    }
}

impl Default for Replica {
    fn default() -> Self {
        let (sender, rx) = mpsc::unbounded_channel::<ReplicaMessage>();

        tokio::spawn(async move {
            let mut rx = rx;
            let mut canisters = HashMap::<Principal, mpsc::UnboundedSender<CanisterMessage>>::new();

            while let Some(m) = rx.recv().await {
                match m {
                    ReplicaMessage::CanisterAdded { canister_id, .. }
                        if canisters.contains_key(&canister_id) =>
                    {
                        panic!(
                            "Canister '{}' is already defined in the replica.",
                            canister_id
                        )
                    }
                    ReplicaMessage::CanisterAdded {
                        canister_id,
                        channel,
                    } => {
                        canisters.insert(canister_id, channel);
                    }
                    ReplicaMessage::CanisterRequest {
                        canister_id,
                        message,
                        reply_sender,
                    } => {
                        if let Some(chan) = canisters.get(&canister_id) {
                            chan.send(CanisterMessage {
                                message,
                                reply_sender,
                            })
                            .unwrap_or_else(|_| {
                                panic!("ic-kit-runtime: Could not enqueue the request.")
                            });
                        } else {
                            let cycles_refunded = match message {
                                Message::CustomTask { env, .. } => env.cycles_available,
                                Message::Request { env, .. } => env.cycles_refunded,
                                Message::Reply { .. } => 0,
                            };

                            reply_sender
                                .unwrap()
                                .send(CallReply::Reject {
                                    rejection_code: RejectionCode::DestinationInvalid,
                                    rejection_message: format!(
                                        "Canister '{}' does not exists",
                                        canister_id
                                    ),
                                    cycles_refunded,
                                })
                                .expect("ic-kit-runtime: Could not send the response.");
                        }
                    }
                    ReplicaMessage::CanisterReply {
                        canister_id,
                        message,
                    } => {
                        let chan = canisters.get(&canister_id).unwrap();
                        chan.send(CanisterMessage {
                            message,
                            reply_sender: None,
                        })
                        .unwrap_or_else(|_| {
                            panic!("ic-kit-runtime: Could not enqueue the response request.")
                        });
                    }
                }
            }
        });

        Replica { sender }
    }
}

impl<'a> CanisterHandle<'a> {
    /// Create a new call builder to call this canister.
    pub fn new_call<S: Into<String>>(&self, method_name: S) -> CallBuilder {
        CallBuilder::new(self.replica, self.canister_id, method_name.into())
    }

    /// Run the given custom function in the execution thread of the canister.
    pub async fn custom<F: FnOnce() + Send + RefUnwindSafe + UnwindSafe + 'static>(
        &self,
        f: F,
        env: Env,
    ) -> CallReply {
        let (tx, rx) = oneshot::channel();

        self.replica.enqueue_request(
            self.canister_id,
            Message::CustomTask {
                request_id: RequestId::new(),
                task: Box::new(f),
                env,
            },
            Some(tx),
        );

        rx.await.unwrap()
    }

    /// Run the given raw message in the canister's execution thread.
    pub async fn run_env(&self, env: Env) -> CallReply {
        let (tx, rx) = oneshot::channel();

        self.replica.enqueue_request(
            self.canister_id,
            Message::Request {
                request_id: RequestId::new(),
                env,
            },
            Some(tx),
        );

        rx.await.unwrap()
    }

    /// Runs the init hook of the canister. For more customization use [`CanisterHandle::run_env`]
    /// with [`Env::init()`].
    pub async fn init(&self) -> CallReply {
        self.run_env(Env::init()).await
    }

    /// Runs the pre_upgrade hook of the canister. For more customization use
    /// [`CanisterHandle::run_env`] with [`Env::pre_upgrade()`].
    pub async fn pre_upgrade(&self) -> CallReply {
        self.run_env(Env::pre_upgrade()).await
    }

    /// Runs the post_upgrade hook of the canister. For more customization use
    /// [`CanisterHandle::run_env`] with [`Env::post_upgrade()`].
    pub async fn post_upgrade(&self) -> CallReply {
        self.run_env(Env::post_upgrade()).await
    }

    /// Runs the post_upgrade hook of the canister. For more customization use
    /// [`CanisterHandle::run_env`] with [`Env::heartbeat()`].
    pub async fn heartbeat(&self) -> CallReply {
        self.run_env(Env::heartbeat()).await
    }
}