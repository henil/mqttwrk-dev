use std::time::Instant;
use std::{fs, io};
use std::sync::Arc;

use crate::Config;

use tokio::{task, pin, time, select};
use tokio::sync::Barrier;
use tokio::time::Duration;
use rumqttc::{MqttOptions, EventLoop, Request, QoS, Incoming, Subscribe, PublishRaw, Sender};
use thiserror::Error;

const ID_PREFIX: &str = "rumqtt";

pub(crate) struct Connection {
    id: String,
    config: Arc<Config>,
    eventloop: EventLoop,
    sink: Option<String>
}

#[derive(Error, Debug)]
pub enum ConnectionError {
    #[error("IO error = {0:?}")]
    Io(#[from] io::Error),
    #[error("Connection error = {0:?}")]
    Connection(#[from] rumqttc::ConnectionError),
    #[error("Wrong packet = {0:?}")]
    WrongPacket(Incoming)
}

impl Connection {
    pub async fn new(id: usize, sink: Option<String>, config: Arc<Config>) -> Result<Connection, ConnectionError> {
        let id = if sink.is_none() {
            format!("{}-{}", ID_PREFIX, id)
        } else {
            format!("{}-sink-{}", ID_PREFIX, id)
        };

        let mut mqttoptions = MqttOptions::new(&id, &config.server, config.port);
        mqttoptions.set_keep_alive(config.keep_alive);
        mqttoptions.set_inflight(config.max_inflight);
        mqttoptions.set_connection_timeout(config.conn_timeout);
        mqttoptions.set_max_request_batch(10);

        if let Some(ca_file) = &config.ca_file {
            let ca = fs::read(ca_file)?;
            mqttoptions.set_ca(ca);
        }

        if let Some(client_cert_file) = &config.client_cert {
            let cert = fs::read(client_cert_file)?;
            let key = fs::read(config.client_key.as_ref().unwrap())?;
            mqttoptions.set_client_auth(cert, key);
        }


        let mut eventloop = EventLoop::new(mqttoptions, 10);
        let requests_tx = eventloop.handle();

        let sconfig = config.clone();
        let ssink = sink.clone();
        let mut subscriber_count = config.subscribers;
        
        if sink.is_some() {
            // subscriber count options are invalidated for sink connections
            subscriber_count = 1;
        }

        task::spawn(async move {
            let qos = get_qos(sconfig.qos);

            // Sink connections contains 1 subscription connections
            match ssink {
                Some(topic) => subscribe(topic.to_owned(), requests_tx.clone(), qos).await,
                None => {
                    // subscribes
                    for i in 0..sconfig.subscribers {
                        // Subscribe to one topic per connection
                        let topic = format!("hello/{}-{}/0/world", ID_PREFIX, i);
                        subscribe(topic, requests_tx.clone(), qos).await;
                    }
                }
            }
        });

        // Handle connection and subscriptions first
        let mut sub_ack_count = 0;
        loop {
            let (incoming, _outgoing) = eventloop.poll().await?;
            if let Some(v) = incoming {
                match v {
                    Incoming::SubAck(_) => sub_ack_count += 1,
                    Incoming::ConnAck(_) => (),
                    incoming => return Err(ConnectionError::WrongPacket(incoming))
                }
            }

            if sub_ack_count >= subscriber_count {
                break
            }
        }

        Ok(Connection {
            id,
            config,
            eventloop,
            sink
        })
    }

    pub async fn start(&mut self, barrier: Arc<Barrier>) {
        // Wait for all the subscription from other connections to finish
        // while doing ping requests so that broker doesn't disconnect
        let barrier = barrier.wait();
        pin!(barrier);
        println!("await barrier = {:?}", self.id);
        loop {
            select! {
                _ = self.eventloop.poll() => {},
                _ = &mut barrier => break,
            }
        } 

        println!("done barrier = {:?}", self.id);
        if self.id == "rumqtt-sink-1" {
            println!("All connections and subscriptions ok");
        }

        let qos = get_qos(self.config.qos);
        let payload_size = self.config.payload_size;
        let count = self.config.count;
        let publishers = self.config.publishers;
        let delay = self.config.delay;
        let id = self.id.clone();
       
        let start = Instant::now();
        let mut acks_count = 0;
        let mut incoming_count = 0;
        let mut acks_expected = self.config.count * self.config.publishers;
        let mut incoming_expected = self.config.count * self.config.publishers * self.config.subscribers;
        let mut outgoing_elapsed = Duration::from_secs(0);
        let mut incoming_elapsed = Duration::from_secs(0);
        let mut outgoing_done = false;
        let mut incoming_done = false;

        // Sink connections are single subscription connections
        if self.sink.is_none() {
            let requests_tx = self.eventloop.requests_tx.clone();
            for i in 0..publishers {
                let topic = format!("hello/{}/{}/world", id, i);
                let tx = requests_tx.clone();
                task::spawn(async move {
                    requests(topic, payload_size, count, tx, qos, delay).await;
                });
            }
        } else {
            acks_expected = 0;
            incoming_expected = self.config.connections * self.config.count * self.config.publishers;
        }


        let mut reconnects: i32 = 0;
        loop {
            let (incoming, _outgoing) = match self.eventloop.poll().await {
                Ok(v) => v,
                Err(e) => {
                    error!("Id = {}, Connection error = {:?}", self.id, e);
                    reconnects += 1;
                    if reconnects == 1 { break }

                    continue;
                }
            };

            // Never exit during idle connection tests
            if self.config.publishers == 0 || self.config.count == 0 {
                continue
            }

            // println!("Id = {}, {:?}", id, incoming);

            if let Some(v) = incoming {
                match v {
                   Incoming::PubAck(_pkid) => acks_count += 1,
                   Incoming::Publish(_publish) => incoming_count += 1,
                   Incoming::PingResp => {},
                   incoming => {
                       error!("Id = {}, Unexpected incoming packet = {:?}", id, incoming);
                       break;
                   }
               }
            }

            if !outgoing_done && acks_count >= acks_expected {
                outgoing_elapsed = start.elapsed();
                outgoing_done = true;
            }

            if !incoming_done && incoming_count >= incoming_expected  {
                incoming_elapsed = start.elapsed();
                incoming_done = true;
            }

            if outgoing_done && incoming_done {
                break
            }
        }

        let outgoing_throughput = (acks_count * 1000) as f32 / outgoing_elapsed.as_millis() as f32;
        let incoming_throughput = (incoming_count * 1000) as f32 / incoming_elapsed.as_millis() as f32;

        println!(
            "Id = {}
            Outgoing publishes : Received = {:<7} Throughput = {} messages/s
            Incoming publishes : Received = {:<7} Throughput = {} messages/s
            Reconnects         : {}",
            self.id,
            acks_count,
            outgoing_throughput,
            incoming_count,
            incoming_throughput,
            reconnects,
        );
    }
}


/// make count number of requests at specified QoS.
async fn requests(topic: String, payload_size: usize, count: usize, requests_tx: Sender<Request>, qos: QoS, delay: u64) {
    let mut interval = match delay {
        0 => None,
        delay => Some(time::interval(time::Duration::from_secs(delay)))
    };

    for _i in 0..count {
        let payload = vec![0; payload_size];
        // payload[0] = (i % 255) as u8;
        let publish = PublishRaw::new(&topic, qos, payload).unwrap();
        let publish = Request::PublishRaw(publish);
        if let Some(interval) = &mut interval {
            interval.tick().await;
        }

        // These errors are usually due to eventloop task being dead. We can ignore the
        // error here as the failed eventloop task would have already printed an error
        if let Err(_e) = requests_tx.send(publish).await {
            break
        }
    }
}

/// create subscriptions for a topic.
async fn subscribe(topic: String, requests_tx: Sender<Request>, qos: QoS) {
    let subscription = Subscribe::new(&topic, qos);
    requests_tx.send(Request::Subscribe(subscription)).await.unwrap();
}

/// get QoS level. Default is AtLeastOnce.
fn get_qos(qos: i16) -> QoS {
    match qos {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        2 => QoS::ExactlyOnce,
        _ => QoS::AtLeastOnce
    }
}

