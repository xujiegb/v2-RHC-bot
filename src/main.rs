use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use regex::Regex;
use reqwest::Client;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    process::Stdio,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    time::{sleep, Duration},
};

const DEFAULT_DATA: &str = "/var/lib/rhc-bot";
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Action>,
    #[arg(long, env="RHC_BOT_DATA", default_value=DEFAULT_DATA)]
    data: PathBuf,
}
#[derive(Subcommand)]
enum Action {
    Run,
    Config,
    Reset,
}
#[derive(Clone, Serialize, Deserialize)]
struct Config {
    token: String,
    max_verifications: u32,
    welcome: String,
    button: String,
    prompt: String,
    timeout_seconds: u64,
    image: String,
    #[serde(default = "default_log_retention_days")]
    log_retention_days: u64,
}
fn default_log_retention_days() -> u64 {
    7
}
impl Default for Config {
    fn default() -> Self {
        Self {
            token: "".into(),
            max_verifications: 2,
            welcome: "欢迎新成员，请在 10 分钟内私聊完成验证。".into(),
            button: "开始验证".into(),
            prompt: "请输入 rhc connect 命令。".into(),
            timeout_seconds: 600,
            image: "registry.access.redhat.com/ubi10/ubi:latest".into(),
            log_retention_days: default_log_retention_days(),
        }
    }
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
fn log(data: &PathBuf, message: &str) {
    let dir = data.join("logs");
    if fs::create_dir_all(&dir).is_ok() {
        let path = dir.join(format!("{}.log", now() / 86_400));
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{} {}", now(), message);
        }
    }
    eprintln!("{message}");
}
fn clean_logs(data: &PathBuf, retention_days: u64) {
    let Ok(entries) = fs::read_dir(data.join("logs")) else {
        return;
    };
    let cutoff = now().saturating_sub((retention_days.saturating_mul(86_400)) as i64);
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .trim_end_matches(".log")
            .parse::<i64>()
            .is_ok_and(|day| day * 86_400 < cutoff)
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}
fn paths(data: &PathBuf) -> (PathBuf, PathBuf) {
    (data.join("config.json"), data.join("state.db"))
}
fn load(data: &PathBuf) -> Result<Config> {
    let (p, _) = paths(data);
    if !p.exists() {
        return Ok(Config::default());
    }
    Ok(serde_json::from_slice(&fs::read(p)?)?)
}
fn db(data: &PathBuf) -> Result<Connection> {
    fs::create_dir_all(data)?;
    let (_, p) = paths(data);
    let c = Connection::open(p)?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    c.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS pending(user_id INTEGER,chat_id INTEGER,deadline INTEGER,failures INTEGER DEFAULT 0,busy INTEGER DEFAULT 0,PRIMARY KEY(user_id,chat_id));")?;
    Ok(c)
}
fn claim_slot(data: &PathBuf, user: i64, chat: i64, limit: u32) -> Result<bool> {
    let mut c = db(data)?;
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE pending SET busy=1 WHERE user_id=?1 AND chat_id=?2 AND busy=0
         AND (SELECT count(*) FROM pending WHERE busy=1) < ?3",
        params![user, chat, limit],
    )?;
    tx.commit()?;
    Ok(changed == 1)
}
async fn api(client: &Client, token: &str, method: &str, body: Value) -> Result<Value> {
    let v: Value = client
        .post(format!("https://api.telegram.org/bot{token}/{method}"))
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if !v["ok"].as_bool().unwrap_or(false) {
        bail!("Telegram: {}", v["description"])
    }
    Ok(v["result"].clone())
}
async fn send(c: &Client, t: &str, id: i64, text: &str) {
    let _ = api(c, t, "sendMessage", json!({"chat_id":id,"text":text})).await;
}
async fn ban(c: &Client, t: &str, chat: i64, user: i64) {
    let _ = api(
        c,
        t,
        "banChatMember",
        json!({"chat_id":chat,"user_id":user,"revoke_messages":true}),
    )
    .await;
}
async fn is_chat_admin(c: &Client, t: &str, chat: i64, user: i64) -> Result<bool> {
    let member = api(
        c,
        t,
        "getChatMember",
        json!({"chat_id":chat,"user_id":user}),
    )
    .await?;
    Ok(matches!(
        member["status"].as_str(),
        Some("administrator" | "creator")
    ))
}
async fn reject(
    data: &PathBuf,
    c: &Client,
    t: &str,
    user: i64,
    chat: i64,
    fail: i64,
) -> Result<()> {
    send(c, t, user, "验证失败").await;
    let d = db(data)?;
    if fail + 1 >= 2 {
        d.execute(
            "DELETE FROM pending WHERE user_id=? AND chat_id=?",
            params![user, chat],
        )?;
        ban(c, t, chat, user).await
    } else {
        d.execute(
            "UPDATE pending SET failures=?,busy=0 WHERE user_id=? AND chat_id=?",
            params![fail + 1, user, chat],
        )?;
    }
    Ok(())
}
async fn verify(
    data: PathBuf,
    c: Client,
    cfg: Config,
    user: i64,
    chat: i64,
    key: String,
    org: String,
    fail: i64,
) -> Result<()> {
    let script = "dnf -y install rhc subscription-manager >/dev/null && read -r KEY && read -r ORG && rhc connect --activation-key=\"$KEY\" --organization=\"$ORG\" >/dev/null && subscription-manager unregister >/dev/null";
    let mut child = Command::new("podman")
        .args(["run", "--rm", "-i", &cfg.image, "bash", "-c", script])
        .stdin(Stdio::piped())
        .spawn()
        .context("无法启动 podman")?;
    let mut stdin = child.stdin.take().context("无法打开 podman stdin")?;
    stdin
        .write_all(format!("{key}\n{org}\n").as_bytes())
        .await?;
    drop(stdin);
    let status = child.wait().await;
    if !matches!(status,Ok(s) if s.success()) {
        return reject(&data, &c, &cfg.token, user, chat, fail).await;
    }
    db(&data)?.execute(
        "DELETE FROM pending WHERE user_id=? AND chat_id=?",
        params![user, chat],
    )?;
    api(&c,&cfg.token,"restrictChatMember",json!({"chat_id":chat,"user_id":user,"permissions":{"can_send_messages":true,"can_send_audios":true,"can_send_documents":true,"can_send_photos":true,"can_send_videos":true,"can_send_video_notes":true,"can_send_voice_notes":true,"can_send_polls":true,"can_send_other_messages":true,"can_add_web_page_previews":true,"can_invite_users":true}})).await?;
    send(&c, &cfg.token, user, "验证成功").await;
    Ok(())
}
async fn run(data: PathBuf) -> Result<()> {
    let client = Client::new();
    let mut offset = 0;
    db(&data)?;
    let cfg = load(&data)?;
    if cfg.token.is_empty() {
        bail!("请先运行 rhc-bot config 设置 token")
    }
    let me = api(&client, &cfg.token, "getMe", json!({})).await?;
    let bot_id = me["id"].as_i64().context("Bot 无 id")?;
    let username = me["username"]
        .as_str()
        .context("Bot 无 username")?
        .to_owned();
    let re = Regex::new(
        r"^rhc connect --activation-key=([A-Za-z0-9-]{20,45}) --organization=([0-9]{1,12})$",
    )?;
    loop {
        let cfg = load(&data)?;
        clean_logs(&data, cfg.log_retention_days);
        {
            let d = db(&data)?;
            let mut st = d.prepare("SELECT user_id,chat_id FROM pending WHERE deadline<?")?;
            let rows = st
                .query_map([now()], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<(i64, i64)>>>()?;
            drop(st);
            for (u, ch) in rows {
                d.execute(
                    "DELETE FROM pending WHERE user_id=? AND chat_id=?",
                    params![u, ch],
                )?;
                ban(&client, &cfg.token, ch, u).await;
            }
        }
        let updates = match api(
            &client,
            &cfg.token,
            "getUpdates",
            json!({"offset":offset,"timeout":10,"allowed_updates":["message"]}),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                log(&data, &format!("poll: {e:#}"));
                sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        for u in updates.as_array().unwrap_or(&vec![]) {
            offset = u["update_id"].as_i64().unwrap_or(0) + 1;
            let m = &u["message"];
            let chat = m["chat"]["id"].as_i64().unwrap_or(0);
            let bot_was_added = m["new_chat_members"].as_array().is_some_and(|members| {
                members
                    .iter()
                    .any(|member| member["id"].as_i64() == Some(bot_id))
            });
            if bot_was_added {
                let inviter = m["from"]["id"].as_i64().unwrap_or(0);
                if !is_chat_admin(&client, &cfg.token, chat, inviter)
                    .await
                    .unwrap_or(false)
                {
                    send(&client, &cfg.token, chat, "只有群管理员可以添加本 Bot。").await;
                    let _ = api(&client, &cfg.token, "leaveChat", json!({"chat_id":chat})).await;
                    continue;
                }
                if !is_chat_admin(&client, &cfg.token, chat, bot_id)
                    .await
                    .unwrap_or(false)
                {
                    send(&client, &cfg.token, chat, "已验证添加者为群管理员。请将本 Bot 设置为管理员，并授予封禁用户和限制成员权限；设置完成后 Bot 才会处理新人验证。").await;
                    continue;
                }
                send(
                    &client,
                    &cfg.token,
                    chat,
                    "管理员身份与 Bot 管理权限验证成功。",
                )
                .await;
            }
            for member in m["new_chat_members"].as_array().unwrap_or(&vec![]) {
                if member["is_bot"].as_bool().unwrap_or(false) {
                    continue;
                }
                if !is_chat_admin(&client, &cfg.token, chat, bot_id)
                    .await
                    .unwrap_or(false)
                {
                    continue;
                }
                let uid = member["id"].as_i64().unwrap();
                db(&data)?.execute(
                    "INSERT OR REPLACE INTO pending VALUES(?,?,?,?,0)",
                    params![uid, chat, now() + cfg.timeout_seconds as i64, 0],
                )?;
                api(&client,&cfg.token,"restrictChatMember",json!({"chat_id":chat,"user_id":uid,"permissions":{"can_send_messages":false},"until_date":now()+cfg.timeout_seconds as i64})).await?;
                api(&client,&cfg.token,"sendMessage",json!({"chat_id":chat,"text":cfg.welcome,"reply_markup":{"inline_keyboard":[[{"text":cfg.button,"url":format!("https://t.me/{username}?start=verify")}]]}})).await?;
            }
            if m["chat"]["type"] != "private" {
                continue;
            }
            let uid = m["from"]["id"].as_i64().unwrap_or(0);
            let text = m["text"].as_str().unwrap_or("");
            let row = {
                let d = db(&data)?;
                let x=d.query_row("SELECT chat_id,failures,busy FROM pending WHERE user_id=? ORDER BY deadline DESC",[uid],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?))).ok();
                x
            };
            if text.starts_with("/start") {
                send(
                    &client,
                    &cfg.token,
                    uid,
                    if row.is_some() {
                        &cfg.prompt
                    } else {
                        "没有待验证请求"
                    },
                )
                .await;
                continue;
            }
            let Some((ch, fail, busy)) = row else {
                continue;
            };
            let Some(cap) = re.captures(text) else {
                reject(&data, &client, &cfg.token, uid, ch, fail).await?;
                continue;
            };
            if busy == 1 || !claim_slot(&data, uid, ch, cfg.max_verifications)? {
                db(&data)?.execute(
                    "DELETE FROM pending WHERE user_id=? AND chat_id=?",
                    params![uid, ch],
                )?;
                ban(&client, &cfg.token, ch, uid).await;
                let _ = api(
                    &client,
                    &cfg.token,
                    "unbanChatMember",
                    json!({"chat_id":ch,"user_id":uid,"only_if_banned":true}),
                )
                .await;
                send(
                    &client,
                    &cfg.token,
                    uid,
                    "当前验证人数已满，请过段时间再进群",
                )
                .await;
                continue;
            }
            send(&client, &cfg.token, uid, "正在验证").await;
            let key = cap[1].to_owned();
            let org = cap[2].to_owned();
            let task_data = data.clone();
            let task_client = client.clone();
            let task_cfg = cfg.clone();
            tokio::spawn(async move {
                if let Err(e) = verify(
                    task_data.clone(),
                    task_client,
                    task_cfg,
                    uid,
                    ch,
                    key,
                    org,
                    fail,
                )
                .await
                {
                    log(&task_data, &format!("verify user={uid} chat={ch}: {e:#}"));
                    if let Ok(d) = db(&task_data) {
                        let _ = d.execute(
                            "UPDATE pending SET busy=0 WHERE user_id=? AND chat_id=?",
                            params![uid, ch],
                        );
                    }
                }
            });
        }
    }
}
fn config(data: &PathBuf) -> Result<()> {
    fs::create_dir_all(data)?;
    let mut c = load(data)?;
    macro_rules! ask {
        ($f:ident,$label:expr) => {{
            print!("{} [{}]: ", $label, c.$f);
            io::stdout().flush()?;
            let mut s = String::new();
            io::stdin().read_line(&mut s)?;
            if !s.trim().is_empty() {
                c.$f = s.trim().parse().context("输入无效")?;
            }
        }};
    }
    ask!(token, "Bot token");
    ask!(max_verifications, "同时验证人数");
    ask!(welcome, "群欢迎语");
    ask!(button, "按钮文案");
    ask!(prompt, "提问文案");
    ask!(timeout_seconds, "验证时长（秒）");
    ask!(image, "UBI 镜像");
    ask!(log_retention_days, "日志保留天数");
    let (p, _) = paths(data);
    fs::write(&p, serde_json::to_vec_pretty(&c)?)?;
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Action::Run) {
        Action::Run => run(cli.data).await,
        Action::Config => config(&cli.data),
        Action::Reset => {
            let (_, p) = paths(&cli.data);
            if p.exists() {
                fs::remove_file(p)?
            }
            println!("数据已重置");
            Ok(())
        }
    }
}
