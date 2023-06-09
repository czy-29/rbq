#![feature(async_closure)]

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    routing::{get, get_service, post},
    Extension, Router,
};
use dashmap::DashMap;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tower::ServiceBuilder;
use tower_http::services::ServeDir;
use tracing::Level;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use ricq::client::{DefaultConnector, NetworkStatus};
use ricq::ext::common::after_login;
use ricq::ext::reconnect::{auto_reconnect, Credential};
use ricq::handler::QEvent;
use ricq::Client;
use ricq::{
    client::event::{
        ClientDisconnect, FriendMessageEvent, GroupMessageEvent, KickedOfflineEvent,
        MSFOfflineEvent,
    },
    msg::{
        elem::{Reply, Text},
        MessageChain,
    },
};
use ricq_axum_api::handler::{bot, password, qrcode};
use ricq_axum_api::processor::Processor;
use ricq_axum_api::u8_protocol::U8Protocol;
use ricq_axum_api::{ClientInfo, RicqAxumApi};

// 默认处理器
struct ClientProcessor(DashMap<(i64, u8), Arc<Client>>);

#[async_trait::async_trait]
impl Processor for ClientProcessor {
    async fn on_login_success(
        &self,
        client: Arc<Client>,
        mut event_receiver: broadcast::Receiver<QEvent>,
        credential: Credential,
        network_join_handle: JoinHandle<()>,
    ) {
        let uin = client.uin().await;
        let protocol = client.version().await.protocol.to_u8();
        self.0.insert((uin, protocol), client.clone());
        after_login(&client).await;

        // 此处暂时先都用println/eprintln，等以后再改为tracing
        tokio::spawn(async move {
            const PING: &str = "rbq-ping";
            let pong_msg = MessageChain::new(Text::new("pong".into()));

            while let Ok(event) = event_receiver.recv().await {
                match event {
                    QEvent::GroupMessage(GroupMessageEvent { inner: msg, client }) => {
                        let group_code = msg.group_code;
                        let msg_text = msg.elements.to_string().trim().to_string();

                        println!(
                            "收到群消息：群名：{}，群名片：{}，消息：{}",
                            msg.group_name, msg.group_card, msg_text
                        );

                        if msg_text == PING {
                            let mut chain = pong_msg.clone();

                            chain.with_reply(Reply {
                                reply_seq: msg.seqs[0],
                                sender: msg.from_uin,
                                time: msg.time,
                                elements: msg.elements,
                            });

                            match client.send_group_message(group_code, chain).await {
                                Ok(mr) => {
                                    println!(
                                        "群消息发送成功：group_code: {}, mr: {:?}",
                                        group_code, mr
                                    )
                                }
                                Err(err) => eprintln!(
                                    "群消息发送失败：group_code: {}, err: {:?}",
                                    group_code, err
                                ),
                            }
                        }
                    }
                    QEvent::FriendMessage(FriendMessageEvent { inner: msg, client }) => {
                        let from_uin = msg.from_uin;
                        let msg_text = msg.elements.to_string().trim().to_string();

                        println!("收到好友消息：QQ号：{}，消息：{}", from_uin, msg_text);

                        if msg_text == PING {
                            match client.send_friend_message(from_uin, pong_msg.clone()).await {
                                Ok(mr) => {
                                    println!("好友消息发送成功：target: {}, mr: {:?}", from_uin, mr)
                                }
                                Err(err) => eprintln!(
                                    "好友消息发送失败：target: {}, err: {:?}",
                                    from_uin, err
                                ),
                            }
                        }
                    }
                    QEvent::Login(uin) => {
                        let subject = "新账号上线";
                        let body = format!("账号{}已上线！", uin);

                        eprintln!("{}：{}", subject, body);

                        println!("正在发送邮件……");
                        match notify_email(subject, &body).await {
                            Ok(resp) => println!("邮件发送成功！服务器回应：{:?}", resp),
                            Err(err) => eprintln!("邮件发送失败：{:?}", err),
                        }
                    }
                    QEvent::KickedOffline(KickedOfflineEvent {
                        inner: rpfo,
                        client,
                    }) => {
                        let subject = format!("账号{}被其他客户端踢下线", client.uin().await);
                        let body = format!("{:?}", rpfo);

                        eprintln!("{}：{}", subject, body);

                        println!("正在发送邮件……");
                        match notify_email(&subject, &body).await {
                            Ok(resp) => println!("邮件发送成功！服务器回应：{:?}", resp),
                            Err(err) => eprintln!("邮件发送失败：{:?}", err),
                        }
                    }
                    QEvent::MSFOffline(MSFOfflineEvent {
                        inner: rmfo,
                        client,
                    }) => {
                        let subject = format!("账号{}被服务端强制下线", client.uin().await);
                        let body = format!("{:?}", rmfo);

                        eprintln!("{}：{}", subject, body);

                        println!("正在发送邮件……");
                        match notify_email(&subject, &body).await {
                            Ok(resp) => println!("邮件发送成功！服务器回应：{:?}", resp),
                            Err(err) => eprintln!("邮件发送失败：{:?}", err),
                        }
                    }
                    QEvent::ClientDisconnect(ClientDisconnect { inner: dr, client }) => {
                        let subject =
                            format!("账号{}因网络原因/客户端主动掉线", client.uin().await);
                        let body = format!("{:?}", dr);

                        eprintln!("{}：{}", subject, body);

                        println!("正在发送邮件……");
                        match notify_email(&subject, &body).await {
                            Ok(resp) => println!("邮件发送成功！服务器回应：{:?}", resp),
                            Err(err) => eprintln!("邮件发送失败：{:?}", err),
                        }
                    }
                    other => {
                        println!("收到其它消息：{:?}", other)
                    }
                }
            }
        });

        // 不阻塞
        tokio::spawn(async move {
            network_join_handle.await.ok();
            auto_reconnect(
                client,
                credential,
                Duration::from_secs(10),
                10,
                DefaultConnector,
            )
            .await;
        });
    }

    async fn list_client(&self) -> Vec<ClientInfo> {
        let mut infos = Vec::new();
        for cli in self.0.iter() {
            let (uin, protocol) = cli.key();
            let client = cli.value();
            infos.push(ClientInfo {
                uin: *uin,
                nick: client.account_info.read().await.nickname.clone(),
                status: client.get_status(),
                protocol: *protocol,
            });
        }
        infos
    }

    async fn delete_client(&self, uin: i64, protocol: u8) {
        if let Some((_, client)) = self.0.remove(&(uin, protocol)) {
            client.stop(NetworkStatus::Stop);
        }
    }
}

#[tokio::main]
async fn main() {
    // 初始化日志
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_timer(tracing_subscriber::fmt::time::OffsetTime::new(
                    time::UtcOffset::__from_hms_unchecked(8, 0, 0),
                    time::macros::format_description!(
                        "[year repr:last_two]-[month]-[day] [hour]:[minute]:[second]"
                    ),
                )),
        )
        .with(
            tracing_subscriber::filter::Targets::new()
                .with_target("main", Level::DEBUG)
                .with_target("ricq", Level::DEBUG)
                .with_target("ricq_axum_api", Level::DEBUG),
        )
        .init();

    let processor = ClientProcessor(Default::default());
    let ricq_axum_api = Arc::new(RicqAxumApi::new(processor));

    let app = Router::new()
        .route("/ping", get(async move || "pong"))
        .nest(
            "/login",
            Router::new()
                .nest(
                    "/qrcode",
                    Router::new()
                        .route("/create", post(qrcode::create::<ClientProcessor>))
                        .route("/list", get(qrcode::list::<ClientProcessor>))
                        .route("/delete", post(qrcode::delete::<ClientProcessor>))
                        .route("/query", post(qrcode::query::<ClientProcessor>)),
                )
                .nest(
                    "/password",
                    Router::new()
                        .route("/create", post(password::login::<ClientProcessor>))
                        .route(
                            "/request_sms",
                            post(password::request_sms::<ClientProcessor>),
                        )
                        .route("/submit_sms", post(password::submit_sms::<ClientProcessor>))
                        .route(
                            "/submit_ticket",
                            post(password::submit_ticket::<ClientProcessor>),
                        )
                        .route("/list", get(password::list::<ClientProcessor>))
                        .route("/delete", post(password::delete::<ClientProcessor>)),
                ),
        )
        .nest(
            "/bot",
            Router::new()
                .route("/list", get(bot::list::<ClientProcessor>))
                .route("/delete", post(bot::delete::<ClientProcessor>)),
        )
        .fallback(get_service(ServeDir::new("static")).handle_error(handle_error))
        .layer(
            ServiceBuilder::new()
                .layer(Extension(ricq_axum_api))
                .into_inner(),
        );
    let addr = SocketAddr::from_str("0.0.0.0:9000").expect("解析地址失败");
    println!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn handle_error(_: std::io::Error) -> impl axum::response::IntoResponse {
    (axum::http::StatusCode::NOT_FOUND, "出错了……")
}

mod creds;

use lettre::{
    message::header::ContentType,
    transport::smtp::{response::Response, Error},
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};

// from_mailbox，to_mailbox，relay_server，get_creds四个函数需要自行提供
async fn notify_email(subject: &str, body: &str) -> Result<Response, Error> {
    let msg = Message::builder()
        .from(creds::from_mailbox())
        .to(creds::to_mailbox())
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(String::from(body))
        .unwrap();

    let mailer: AsyncSmtpTransport<Tokio1Executor> =
        AsyncSmtpTransport::<Tokio1Executor>::relay(creds::relay_server())?
            .credentials(creds::get_creds())
            .build();

    mailer.send(msg).await
}
