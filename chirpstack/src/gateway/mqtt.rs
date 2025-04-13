use std::collections::HashMap;
use std::future::Future;
use std::io::Cursor;

use std::sync::RwLock;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chirpstack_api::bs::ProtoBasestationMessage;
use chrono::Utc;
use futures::future::BoxFuture;
use handlebars::Handlebars;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prost::Message;
use rand::Rng;
use rumqttc::tokio_rustls::rustls;
use rumqttc::v5::mqttbytes::v5::{ConnectReturnCode, Publish};
use rumqttc::v5::EventLoop;
use rumqttc::v5::{mqttbytes::QoS, AsyncClient, Event, Incoming, MqttOptions};
use rumqttc::Transport;
use serde::Serialize;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::time::sleep;
use tracing::{error, info, trace};

use crate::config::{BasestationBackendMqtt, GatewayBackendMqtt};
use crate::helpers::tls22::{get_root_certs, load_cert, load_key};
use crate::monitoring::prometheus;
use crate::{downlink, uplink};
use lrwn::region::CommonName;

use super::backend::GatewayBackend;
use super::basestation_backend::BasestationBackend;

#[derive(Clone, Hash, PartialEq, Eq, EncodeLabelSet, Debug)]
struct EventLabels {
    event: String,
}

#[derive(Clone, Hash, PartialEq, Eq, EncodeLabelSet, Debug)]
struct CommandLabels {
    command: String,
}

lazy_static! {
    static ref EVENT_COUNTER: Family<EventLabels, Counter> = {
        let counter = Family::<EventLabels, Counter>::default();
        prometheus::register(
            "gateway_backend_mqtt_events",
            "Number of events received",
            counter.clone(),
        );
        counter
    };
    static ref COMMAND_COUNTER: Family<CommandLabels, Counter> = {
        let counter = Family::<CommandLabels, Counter>::default();
        prometheus::register(
            "gateway_backend_mqtt_commands",
            "Number of commands sent",
            counter.clone(),
        );
        counter
    };
    static ref GATEWAY_JSON: RwLock<HashMap<String, bool>> = RwLock::new(HashMap::new());
}

pub struct MqttBackend<'a> {
    client: AsyncClient,
    templates: handlebars::Handlebars<'a>,
    qos: QoS,
    v4_migrate: bool,
    region_config_id: String,
}

#[derive(Serialize)]
struct CommandTopicContext {
    pub gateway_id: String,
    pub command: String,
}

impl<'a> MqttBackend<'a> {
    async fn new_client(
        region_config_id: &str,
        templates: handlebars::Handlebars<'a>,
        client_id: &str,
        qos: usize,
        server: &str,
        clean_session: bool,
        keep_alive_interval: Duration,
        username: &str,
        password: &str,
        ca_cert: &str,
        tls_cert: &str,
        tls_key: &str,
        v4_migrate: bool,
    ) -> Result<(MqttBackend<'a>, EventLoop)> {
        // get client id, this will generate a random client_id when no client_id has been configured.
        let client_id = if client_id.is_empty() {
            let mut rnd = rand::rng();
            let client_id: u64 = rnd.random();
            format!("{:x}", client_id)
        } else {
            client_id.to_string()
        };

        let qos = match qos {
            0 => QoS::AtMostOnce,
            1 => QoS::AtLeastOnce,
            2 => QoS::ExactlyOnce,
            _ => return Err(anyhow!("Invalid QoS: {}", qos)),
        };

        // Create client
        let mut mqtt_opts = MqttOptions::parse_url(format!("{}?client_id={}", server, client_id))?;
        mqtt_opts.set_clean_start(clean_session);
        mqtt_opts.set_keep_alive(keep_alive_interval);
        if !username.is_empty() || !password.is_empty() {
            mqtt_opts.set_credentials(username, password);
        }

        if !ca_cert.is_empty() || !tls_cert.is_empty() || !tls_key.is_empty() {
            info!(
                "Configuring client with TLS certificate, ca_cert: {}, tls_cert: {}, tls_key: {}",
                ca_cert, tls_cert, tls_key
            );

            let root_certs = get_root_certs(if ca_cert.is_empty() {
                None
            } else {
                Some(ca_cert.to_string())
            })?;

            let client_conf = if tls_cert.is_empty() && tls_key.is_empty() {
                rustls::ClientConfig::builder()
                    .with_root_certificates(root_certs.clone())
                    .with_no_client_auth()
            } else {
                rustls::ClientConfig::builder()
                    .with_root_certificates(root_certs.clone())
                    .with_client_auth_cert(load_cert(&tls_cert).await?, load_key(&tls_key).await?)?
            };

            mqtt_opts.set_transport(Transport::tls_with_config(client_conf.into()));
        }

        let (client, eventloop) = AsyncClient::new(mqtt_opts, 100);

        let b = MqttBackend {
            client,
            qos,
            templates,
            v4_migrate,
            region_config_id: region_config_id.to_string(),
        };

        // connect
        info!(region_id = %region_config_id, server_uri = %server, clean_session = clean_session, client_id = %client_id, "Connecting to MQTT broker");
        Ok((b, eventloop))
    }

    pub async fn new(
        region_config_id: &str,
        region_common_name: CommonName,
        conf: &GatewayBackendMqtt,
    ) -> Result<MqttBackend<'a>> {
        // topic templates
        let mut templates = Handlebars::new();
        templates.register_template_string(
            "command_topic",
            if conf.command_topic.is_empty() {
                let command_topic = "gateway/{{ gateway_id }}/command/{{ command }}".to_string();
                if conf.topic_prefix.is_empty() {
                    command_topic
                } else {
                    format!("{}/{}", conf.topic_prefix, command_topic)
                }
            } else {
                conf.command_topic.clone()
            },
        )?;

        let (b, eventloop) = MqttBackend::new_client(
            region_config_id,
            templates,
            &conf.client_id,
            conf.qos,
            &conf.server,
            conf.clean_session,
            conf.keep_alive_interval,
            &conf.username,
            &conf.password,
            &conf.ca_cert,
            &conf.tls_cert,
            &conf.tls_key,
            conf.v4_migrate,
        )
        .await?;

        // Create connect channel
        // We need to re-subscribe on (re)connect to be sure we have a subscription. Even
        // in case of a persistent MQTT session, there is no guarantee that the MQTT persisted the
        // session and that a re-connect would recover the subscription.
        let (connect_tx, connect_rx) = mpsc::channel(10);

        let event_topic = if conf.event_topic.is_empty() {
            let event_topic = "gateway/+/event/+".to_string();
            if conf.topic_prefix.is_empty() {
                event_topic
            } else {
                format!("{}/{}", conf.topic_prefix, event_topic)
            }
        } else {
            conf.event_topic.clone()
        };

        // (Re)subscribe loop
        b.spawn_subscribe_loop(event_topic, &conf.share_name, connect_rx);

        // Eventloop
        b.spawn_event_loop(region_common_name, eventloop, connect_tx, gateway_callback);
        // return backend
        Ok(b)
    }

    pub async fn new_basestation(
        region_config_id: &str,
        region_common_name: CommonName,
        conf: &BasestationBackendMqtt,
    ) -> Result<MqttBackend<'a>> {
        // topic templates
        let mut templates = Handlebars::new();
        templates.register_template_string(
            "command_topic",
            if conf.command_topic.is_empty() {
                let command_topic = "gateway/{{ gateway_id }}/command/{{ command }}".to_string();
                if conf.topic_prefix.is_empty() {
                    command_topic
                } else {
                    format!("{}/{}", conf.topic_prefix, command_topic)
                }
            } else {
                conf.command_topic.clone()
            },
        )?;

        templates.register_template_string(
            "response_topic",
            if conf.response_topic.is_empty() {
                let response_topic = "bssci/{{ gateway_id }}/response/{{ command }}".to_string();
                if conf.topic_prefix.is_empty() {
                    response_topic
                } else {
                    format!("{}/{}", conf.topic_prefix, response_topic)
                }
            } else {
                conf.response_topic.clone()
            },
        )?;

        let (b, eventloop) = MqttBackend::new_client(
            region_config_id,
            templates,
            &conf.client_id,
            conf.qos,
            &conf.server,
            conf.clean_session,
            conf.keep_alive_interval,
            &conf.username,
            &conf.password,
            &conf.ca_cert,
            &conf.tls_cert,
            &conf.tls_key,
            false,
        )
        .await?;

        // Create connect channel
        // We need to re-subscribe on (re)connect to be sure we have a subscription. Even
        // in case of a persistent MQTT session, there is no guarantee that the MQTT persisted the
        // session and that a re-connect would recover the subscription.
        let (connect_tx, connect_rx) = mpsc::channel(10);

        let event_topic = if conf.event_topic.is_empty() {
            let event_topic = "basestation/+/event/+".to_string();
            if conf.topic_prefix.is_empty() {
                event_topic
            } else {
                format!("{}/{}", conf.topic_prefix, event_topic)
            }
        } else {
            conf.event_topic.clone()
        };

        // (Re)subscribe loop
        b.spawn_subscribe_loop(event_topic, &conf.share_name, connect_rx);

        // Eventloop
        b.spawn_event_loop(region_common_name, eventloop, connect_tx, basestation_callback);

        // return backend
        Ok(b)
    }

    /// Spawn the (re)subscribe loop
    fn spawn_subscribe_loop(
        &self,
        event_topic: String,
        share_name: &str,
        mut connect_rx: Receiver<bool>,
    ) {
        tokio::spawn({
            let client = self.client.clone();
            let qos = self.qos;
            let region_config_id = self.region_config_id.clone();
            let share_name = share_name.to_string();

            async move {
                while let Some(shared_sub_support) = connect_rx.recv().await {
                    let event_topic = if shared_sub_support {
                        format!("$share/{}/{}", share_name, event_topic)
                    } else {
                        event_topic.clone()
                    };

                    info!(region_id = %region_config_id, event_topic = %event_topic, "Subscribing to gateway event topic");
                    if let Err(e) = client.subscribe(&event_topic, qos).await {
                        error!(region_id = %region_config_id, event_topic = %event_topic, error = %e, "MQTT subscribe error");
                    }
                }
            }
        });
    }

    /// Spawn the event loop and register the callback function
    fn spawn_event_loop<Callback, Fut>(
        &self,
        region_common_name: CommonName,
        mut eventloop: EventLoop,
        connect_tx: Sender<bool>,
        f: Callback,
    ) where
        Callback: Fn(bool, String, CommonName, Publish) -> Fut + Send + Clone + 'static,
        Fut: Future<Output = ()> + std::marker::Send,
    {
        tokio::spawn({
            let region_config_id = self.region_config_id.clone();
            let v4_migrate = self.v4_migrate.clone();
            let callback = f.clone();

            async move {
                info!("Starting MQTT event loop");

                loop {
                    match eventloop.poll().await {
                        Ok(v) => {
                            trace!(event = ?v, "MQTT event");

                            match v {
                                Event::Incoming(Incoming::Publish(p)) => {
                                    callback(
                                        v4_migrate,
                                        region_config_id.clone(),
                                        region_common_name,
                                        p,
                                    )
                                    .await
                                }
                                Event::Incoming(Incoming::ConnAck(v)) => {
                                    if v.code == ConnectReturnCode::Success {
                                        // Per specification:
                                        // A value of 1 means Shared Subscriptions are supported. If not present, then Shared Subscriptions are supported.
                                        let shared_sub_support = v
                                            .properties
                                            .map(|v| {
                                                v.shared_subscription_available
                                                    .map(|v| v == 1)
                                                    .unwrap_or(true)
                                            })
                                            .unwrap_or(true);

                                        if let Err(e) = connect_tx.try_send(shared_sub_support) {
                                            error!(error = %e, "Send to subscribe channel error");
                                        }
                                    } else {
                                        error!(code = ?v.code, "Connection error");
                                        sleep(Duration::from_secs(1)).await
                                    }
                                }
                                _ => {}
                            }
                        }
                        Err(e) => {
                            error!(error = %e, "MQTT error");
                            sleep(Duration::from_secs(1)).await
                        }
                    }
                }
            }
        });
    }

    fn get_command_topic(&self, gateway_id: &str, command: &str) -> Result<String> {
        Ok(self.templates.render(
            "command_topic",
            &CommandTopicContext {
                gateway_id: gateway_id.to_string(),
                command: command.to_string(),
            },
        )?)
    }

    fn get_response_topic(&self, gateway_id: &str, command: &str) -> Result<String> {
        Ok(self.templates.render(
            "response_topic",
            &CommandTopicContext {
                gateway_id: gateway_id.to_string(),
                command: command.to_string(),
            },
        )?)
    }
}

#[async_trait]
impl GatewayBackend for MqttBackend<'_> {
    async fn send_downlink(&self, df: &chirpstack_api::gw::DownlinkFrame) -> Result<()> {
        COMMAND_COUNTER
            .get_or_create(&CommandLabels {
                command: "down".to_string(),
            })
            .inc();
        let topic = self.get_command_topic(&df.gateway_id, "down")?;
        let mut df = df.clone();

        if self.v4_migrate {
            df.v4_migrate();
        }

        let json = gateway_is_json(&df.gateway_id);
        let b = match json {
            true => serde_json::to_vec(&df)?,
            false => df.encode_to_vec(),
        };

        info!(region_id = %self.region_config_id, gateway_id = %df.gateway_id, topic = %topic, json = json, "Sending downlink frame");
        self.client.publish(topic, self.qos, false, b).await?;
        trace!("Message published");

        Ok(())
    }

    async fn send_configuration(
        &self,
        gw_conf: &chirpstack_api::gw::GatewayConfiguration,
    ) -> Result<()> {
        COMMAND_COUNTER
            .get_or_create(&CommandLabels {
                command: "config".to_string(),
            })
            .inc();
        let topic = self.get_command_topic(&gw_conf.gateway_id, "config")?;
        let json = gateway_is_json(&gw_conf.gateway_id);
        let b = match json {
            true => serde_json::to_vec(&gw_conf)?,
            false => gw_conf.encode_to_vec(),
        };

        info!(region_id = %self.region_config_id, gateway_id = %gw_conf.gateway_id, topic = %topic, json = json, "Sending gateway configuration");
        self.client.publish(topic, self.qos, false, b).await?;
        trace!("Message published");

        Ok(())
    }
}

async fn gateway_callback(
    v4_migrate: bool,
    region_config_id: String,
    region_common_name: CommonName,
    p: Publish,
) {
    let topic = String::from_utf8_lossy(&p.topic);

    let err = || -> Result<()> {
        let json = payload_is_json(&p.payload);

        info!(
            region_id = region_config_id,
            topic = %topic,
            qos = ?p.qos,
            json = json,
            "Message received from gateway"
        );

        if topic.ends_with("/up") {
            EVENT_COUNTER
                .get_or_create(&EventLabels {
                    event: "up".to_string(),
                })
                .inc();
            let mut event = match json {
                true => serde_json::from_slice(&p.payload)?,
                false => chirpstack_api::gw::UplinkFrame::decode(&mut Cursor::new(&p.payload))?,
            };

            if v4_migrate {
                event.v4_migrate();
            }

            if let Some(rx_info) = &mut event.rx_info {
                set_gateway_json(&rx_info.gateway_id, json);
                rx_info.ns_time = Some(Utc::now().into());
            }

            tokio::spawn(uplink::deduplicate_uplink(
                region_common_name,
                region_config_id.to_string(),
                event,
            ));
        } else if topic.ends_with("/stats") {
            EVENT_COUNTER
                .get_or_create(&EventLabels {
                    event: "stats".to_string(),
                })
                .inc();
            let mut event = match json {
                true => serde_json::from_slice(&p.payload)?,
                false => chirpstack_api::gw::GatewayStats::decode(&mut Cursor::new(&p.payload))?,
            };

            if v4_migrate {
                event.v4_migrate();
            }

            event
                .metadata
                .insert("region_config_id".to_string(), region_config_id.to_string());
            event.metadata.insert(
                "region_common_name".to_string(),
                region_common_name.to_string(),
            );
            set_gateway_json(&event.gateway_id, json);
            tokio::spawn(uplink::stats::Stats::handle(event));
        } else if topic.ends_with("/ack") {
            EVENT_COUNTER
                .get_or_create(&EventLabels {
                    event: "ack".to_string(),
                })
                .inc();
            let mut event = match json {
                true => serde_json::from_slice(&p.payload)?,
                false => chirpstack_api::gw::DownlinkTxAck::decode(&mut Cursor::new(&p.payload))?,
            };

            if v4_migrate {
                event.v4_migrate();
            }

            set_gateway_json(&event.gateway_id, json);
            tokio::spawn(downlink::tx_ack::TxAck::handle(event));
        } else if topic.ends_with("/mesh-heartbeat") {
            EVENT_COUNTER
                .get_or_create(&EventLabels {
                    event: "mesh-heartbeat".to_string(),
                })
                .inc();
            let event = match json {
                true => serde_json::from_slice(&p.payload)?,
                false => chirpstack_api::gw::MeshHeartbeat::decode(&mut Cursor::new(&p.payload))?,
            };

            tokio::spawn(uplink::mesh::MeshHeartbeat::handle(event));
        } else {
            return Err(anyhow!("Unknown event type"));
        }

        Ok(())
    }()
    .err();

    if err.is_some() {
        error!(
            region_id = %region_config_id,
            topic = %topic,
            qos = ?p.qos,
            "Processing gateway event error: {}",
            err.as_ref().unwrap()
        );
    }
}


#[async_trait]
impl BasestationBackend for MqttBackend<'_> {
    async fn send_command(&self, cmd: &chirpstack_api::bs::ProtoCommand) -> Result<()> {
        todo!();
    }

    async fn send_response(&self,  rsp: &chirpstack_api::bs::ProtoResponse) -> Result<()> {
        todo!();
    }
    
}

async fn basestation_callback(
    v4_migrate: bool,
    region_config_id: String,
    region_common_name: CommonName,
    p: Publish,
) {
    let _ = v4_migrate;
    let topic = String::from_utf8_lossy(&p.topic);

    let err = || -> Result<()> {
        let json = payload_is_json(&p.payload);

        info!(
            region_id = region_config_id,
            topic = %topic,
            qos = ?p.qos,
            json = json,
            "Message received from gateway"
        );

        if topic.contains("/bs/") {
            // this should be a ProtoBasestationMessage, event type can be derived from the contents instead of checking the subtopic

            let event = match json {
                true => serde_json::from_slice(&p.payload)?,
                false => chirpstack_api::bs::ProtoBasestationMessage::decode(&mut Cursor::new(
                    &p.payload,
                ))?,
            };

            if let Some(v1) = event.v1 {
                match v1 {
                    chirpstack_api::bs::proto_basestation_message::V1::Con(
                        basestation_connection,
                    ) => {
                        EVENT_COUNTER
                            .get_or_create(&EventLabels {
                                event: "bs/con".to_string(),
                            })
                            .inc();
                    }
                    chirpstack_api::bs::proto_basestation_message::V1::Status(
                        basestation_status,
                    ) => {
                        EVENT_COUNTER
                            .get_or_create(&EventLabels {
                                event: "bs/status".to_string(),
                            })
                            .inc();
                    }
                    chirpstack_api::bs::proto_basestation_message::V1::VmStatus(
                        basestation_variable_mac_status,
                    ) => {
                        EVENT_COUNTER
                            .get_or_create(&EventLabels {
                                event: "bs/vm_status".to_string(),
                            })
                            .inc();
                    }
                }
            } else {
                return Err(anyhow!("Unknown basestation event type"));
            }
        } else if topic.contains("/ep/") {
            // this should be a ProtoEndnodeMessage

            let event = match json {
                true => serde_json::from_slice(&p.payload)?,
                false => {
                    chirpstack_api::bs::ProtoEndnodeMessage::decode(&mut Cursor::new(&p.payload))?
                }
            };

            if let Some(v1) = event.v1 {
                match v1 {
                    chirpstack_api::bs::proto_endnode_message::V1::Att(endnode_att_message) => {
                        EVENT_COUNTER
                        .get_or_create(&EventLabels {
                            event: "ep/att".to_string(),
                        })
                        .inc();
                    }
                    chirpstack_api::bs::proto_endnode_message::V1::Det(endnode_det_message) => {
                        EVENT_COUNTER
                        .get_or_create(&EventLabels {
                            event: "ep/det".to_string(),
                        })
                        .inc();
                    }
                    chirpstack_api::bs::proto_endnode_message::V1::UlData(
                        endnode_ul_data_message,
                    ) => {
                        EVENT_COUNTER
                        .get_or_create(&EventLabels {
                            event: "ep/ul_data".to_string(),
                        })
                        .inc();
                    }
                    chirpstack_api::bs::proto_endnode_message::V1::DlRxStat(
                        endnode_downlink_rx_status,
                    ) => {
                        EVENT_COUNTER
                        .get_or_create(&EventLabels {
                            event: "ep/rx_status".to_string(),
                        })
                        .inc();
                    }
                    chirpstack_api::bs::proto_endnode_message::V1::DlRes(
                        endnode_downlink_result,
                    ) => {
                        EVENT_COUNTER
                        .get_or_create(&EventLabels {
                            event: "ep/dl_result".to_string(),
                        })
                        .inc();
                    }
                    chirpstack_api::bs::proto_endnode_message::V1::VmUlData(
                        endnode_variable_mac_ul_data_message,
                    ) => {
                        EVENT_COUNTER
                        .get_or_create(&EventLabels {
                            event: "ep/vm_ul_data".to_string(),
                        })
                        .inc();
                    }

                }
            } else {
                return Err(anyhow!("Unknown endpoint event type"));
            }

           
        } else {
            return Err(anyhow!("Unknown event type"));
        }

        Ok(())
    }()
    .err();

    if err.is_some() {
        error!(
            region_id = %region_config_id,
            topic = %topic,
            qos = ?p.qos,
            "Processing gateway event error: {}",
            err.as_ref().unwrap()
        );
    }
}

fn gateway_is_json(gateway_id: &str) -> bool {
    let gw_json_r = GATEWAY_JSON.read().unwrap();
    gw_json_r.get(gateway_id).cloned().unwrap_or(false)
}

fn set_gateway_json(gateway_id: &str, is_json: bool) {
    let mut gw_json_w = GATEWAY_JSON.write().unwrap();
    gw_json_w.insert(gateway_id.to_string(), is_json);
}

fn payload_is_json(b: &[u8]) -> bool {
    String::from_utf8_lossy(b).contains("gatewayId") || String::from_utf8_lossy(b).contains("bsEui")
}
