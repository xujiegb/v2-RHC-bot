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
const SERVICE_NAME: &str = "rhc-bot.service";
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
    CacheUpdate,
    /// 管理 Telegram 数字 ID 白名单
    Whitelist {
        #[command(subcommand)]
        command: WhitelistAction,
    },
    /// 启动 rhc-bot systemd 服务
    Start,
    /// 停止 rhc-bot systemd 服务
    Stop,
    /// 重启 rhc-bot systemd 服务
    Restart,
    /// 设置 rhc-bot 服务开机自启
    Enable,
    /// 关闭 rhc-bot 服务开机自启
    Disable,
}
#[derive(Subcommand)]
enum WhitelistAction {
    /// 添加白名单 Telegram 数字 ID
    Add { tg_id: i64 },
    /// 删除指定 Telegram 数字 ID；不指定 ID 并加 --all 则删除全部
    Delete {
        tg_id: Option<i64>,
        #[arg(long)]
        all: bool,
    },
    /// 查询白名单 Telegram 数字 ID
    List,
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
    #[serde(default = "default_image_mode")]
    image_mode: String,
    #[serde(default = "default_cache_update_interval")]
    cache_update_interval: String,
    #[serde(default = "default_log_retention_days")]
    log_retention_days: u64,
}
fn default_image_mode() -> String {
    "live".into()
}
fn default_cache_update_interval() -> String {
    "24h".into()
}
fn default_log_retention_days() -> u64 {
    7
}
impl Default for Config {
    fn default() -> Self {
        Self {
            token: "".into(),
            max_verifications: 2,
            welcome: "欢迎新成员，请在 10 分钟内私聊完成验证。\nWelcome. Please complete verification by private message within 10 minutes.".into(),
            button: "开始验证".into(),
            prompt: "请输入 rhc connect 或 subscription-manager register 命令。\nEnter an rhc connect or subscription-manager register command.".into(),
            timeout_seconds: 600,
            image: "registry.access.redhat.com/ubi10/ubi:latest".into(),
            image_mode: default_image_mode(),
            cache_update_interval: default_cache_update_interval(),
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
fn welcome_text(template: &str, member: &Value) -> String {
    let first = member["first_name"].as_str().unwrap_or("");
    let last = member["last_name"].as_str().unwrap_or("");
    let fullname = match (first.is_empty(), last.is_empty()) {
        (false, false) => format!("{first} {last}"),
        (false, true) => first.to_owned(),
        (true, false) => last.to_owned(),
        (true, true) => member["username"].as_str().unwrap_or("").to_owned(),
    };
    template.replace("{fullname}", &fullname)
}
fn verification_credentials(text: &str) -> Option<(String, String)> {
    let rhc = Regex::new(
        r"^rhc connect --activation-key=([A-Za-z0-9-]{20,45}) --organization=([0-9]{1,12})$",
    )
    .ok()?;
    let subscription_manager = Regex::new(
        r"^subscription-manager register --activationkey=([A-Za-z0-9-]{20,45}) --org=([0-9]{1,12})$",
    )
    .ok()?;
    let captures = rhc
        .captures(text)
        .or_else(|| subscription_manager.captures(text))?;
    Some((captures[1].to_owned(), captures[2].to_owned()))
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
    c.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS pending(user_id INTEGER,chat_id INTEGER,deadline INTEGER,failures INTEGER DEFAULT 0,busy INTEGER DEFAULT 0,PRIMARY KEY(user_id,chat_id)); CREATE TABLE IF NOT EXISTS whitelist(tg_id INTEGER PRIMARY KEY,created_at INTEGER NOT NULL);")?;
    Ok(c)
}
fn is_whitelisted(data: &PathBuf, tg_id: i64) -> Result<bool> {
    let c = db(data)?;
    let count: i64 = c.query_row(
        "SELECT count(*) FROM whitelist WHERE tg_id=?1",
        [tg_id],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}
fn whitelist_add(data: &PathBuf, tg_id: i64) -> Result<()> {
    db(data)?.execute(
        "INSERT OR REPLACE INTO whitelist(tg_id,created_at) VALUES(?1,?2)",
        params![tg_id, now()],
    )?;
    Ok(())
}
fn whitelist_delete(data: &PathBuf, tg_id: i64) -> Result<usize> {
    Ok(db(data)?.execute("DELETE FROM whitelist WHERE tg_id=?1", [tg_id])?)
}
fn whitelist_clear(data: &PathBuf) -> Result<usize> {
    Ok(db(data)?.execute("DELETE FROM whitelist", [])?)
}
fn whitelist_list(data: &PathBuf) -> Result<Vec<i64>> {
    let c = db(data)?;
    let mut st = c.prepare("SELECT tg_id FROM whitelist ORDER BY tg_id")?;
    let ids = st
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(ids)
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
    send(c, t, user, "验证失败\nVerification failed").await;
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
fn interval_seconds(value: &str) -> Result<i64> {
    let re = Regex::new(r"^([1-9][0-9]*)([hd])$")?;
    let captures = re.captures(value).context("更新时间必须类似 21h 或 30d")?;
    let number: i64 = captures[1].parse()?;
    Ok(number * if &captures[2] == "h" { 3_600 } else { 86_400 })
}
async fn update_cached_image(data: &PathBuf, cfg: &Config, force: bool) -> Result<()> {
    let marker = data.join("ubi-cache-updated");
    let due = force
        || fs::read_to_string(&marker)
            .ok()
            .and_then(|x| x.parse::<i64>().ok())
            .is_none_or(|last| {
                now() - last >= interval_seconds(&cfg.cache_update_interval).unwrap_or(86_400)
            });
    if due {
        let status = Command::new("podman")
            .args(["pull", &cfg.image])
            .status()
            .await?;
        if !status.success() {
            bail!("无法更新 UBI 缓存镜像")
        }
        fs::create_dir_all(data)?;
        fs::write(marker, now().to_string())?;
    }
    Ok(())
}
async fn systemctl(action: &str) -> Result<()> {
    let status = Command::new("systemctl")
        .args([action, SERVICE_NAME])
        .status()
        .await
        .with_context(|| format!("无法运行 systemctl {action}"))?;
    if !status.success() {
        bail!("systemctl {action} {SERVICE_NAME} 执行失败，请确认当前用户有管理服务的权限")
    }
    Ok(())
}
async fn verification_image(
    data: &PathBuf,
    cfg: &Config,
    user: i64,
    chat: i64,
) -> Result<(String, bool)> {
    if cfg.image_mode == "live" {
        let status = Command::new("podman")
            .args(["pull", &cfg.image])
            .status()
            .await?;
        if !status.success() {
            bail!("无法实时拉取 UBI 镜像")
        }
        return Ok((cfg.image.clone(), false));
    }
    update_cached_image(data, cfg, false).await?;
    let temporary = format!("localhost/rhc-bot-verify-{user}-{chat}-{}", now());
    let status = Command::new("podman")
        .args(["tag", &cfg.image, &temporary])
        .status()
        .await?;
    if !status.success() {
        bail!("无法复制本地 UBI 缓存镜像")
    }
    Ok((temporary, true))
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
    // UBI 10 contains subscription-manager but not rhc. Both accepted input
    // syntaxes are normalized to this command, while credentials remain on stdin.
    let script = "read -r KEY && read -r ORG && trap 'subscription-manager unregister >/dev/null 2>&1 || true; subscription-manager clean >/dev/null 2>&1 || true' EXIT && subscription-manager register --activationkey=\"$KEY\" --org=\"$ORG\" >/dev/null";
    let (image, remove_image) = verification_image(&data, &cfg, user, chat).await?;
    let mut child = Command::new("podman")
        .args(["run", "--rm", "-i", &image, "bash", "-c", script])
        .stdin(Stdio::piped())
        .spawn()
        .context("无法启动 podman")?;
    let mut stdin = child.stdin.take().context("无法打开 podman stdin")?;
    stdin
        .write_all(format!("{key}\n{org}\n").as_bytes())
        .await?;
    drop(stdin);
    let status = child.wait().await;
    if remove_image {
        let _ = Command::new("podman")
            .args(["image", "rm", &image])
            .status()
            .await;
    }
    if !matches!(status,Ok(s) if s.success()) {
        return reject(&data, &c, &cfg.token, user, chat, fail).await;
    }
    db(&data)?.execute(
        "DELETE FROM pending WHERE user_id=? AND chat_id=?",
        params![user, chat],
    )?;
    api(&c,&cfg.token,"restrictChatMember",json!({"chat_id":chat,"user_id":user,"permissions":{"can_send_messages":true,"can_send_audios":true,"can_send_documents":true,"can_send_photos":true,"can_send_videos":true,"can_send_video_notes":true,"can_send_voice_notes":true,"can_send_polls":true,"can_send_other_messages":true,"can_add_web_page_previews":true,"can_invite_users":true}})).await?;
    send(&c, &cfg.token, user, "验证成功\nVerification successful").await;
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
                if is_whitelisted(&data, uid)? {
                    let welcome = welcome_text(&cfg.welcome, member);
                    api(
                        &client,
                        &cfg.token,
                        "sendMessage",
                        json!({"chat_id":chat,"text":welcome}),
                    )
                    .await?;
                    continue;
                }
                db(&data)?.execute(
                    "INSERT OR REPLACE INTO pending VALUES(?,?,?,?,0)",
                    params![uid, chat, now() + cfg.timeout_seconds as i64, 0],
                )?;
                api(&client,&cfg.token,"restrictChatMember",json!({"chat_id":chat,"user_id":uid,"permissions":{"can_send_messages":false},"until_date":now()+cfg.timeout_seconds as i64})).await?;
                let welcome = welcome_text(&cfg.welcome, member);
                api(&client,&cfg.token,"sendMessage",json!({"chat_id":chat,"text":welcome,"reply_markup":{"inline_keyboard":[[{"text":cfg.button,"url":format!("https://t.me/{username}?start=verify")}]]}})).await?;
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
                        "没有待验证请求\nNo pending verification request"
                    },
                )
                .await;
                continue;
            }
            let Some((ch, fail, busy)) = row else {
                continue;
            };
            let Some((key, org)) = verification_credentials(text) else {
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
                    "当前验证人数已满，请过段时间再进群\nVerification capacity is full. Please join the group again later.",
                )
                .await;
                continue;
            }
            send(
                &client,
                &cfg.token,
                uid,
                "正在验证\nVerification in progress",
            )
            .await;
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
    ask!(welcome, "群欢迎语（支持 {fullname}，使用 \\n 换行）");
    c.welcome = c.welcome.replace("\\n", "\n");
    ask!(button, "按钮文案");
    ask!(prompt, "提问文案（使用 \\n 换行）");
    c.prompt = c.prompt.replace("\\n", "\n");
    ask!(timeout_seconds, "验证时长（秒）");
    ask!(image, "UBI 镜像");
    ask!(image_mode, "镜像模式（live 实时拉取/cache 本地缓存）");
    if !matches!(c.image_mode.as_str(), "live" | "cache") {
        bail!("镜像模式只能是 live 或 cache")
    }
    ask!(cache_update_interval, "缓存自动更新时间（例如 21h/30d）");
    interval_seconds(&c.cache_update_interval)?;
    ask!(log_retention_days, "日志保留天数");
    let (p, _) = paths(data);
    fs::write(&p, serde_json::to_vec_pretty(&c)?)?;
    Ok(())
}
async fn menu(data: &PathBuf) -> Result<()> {
    loop {
        let c = load(data)?;
        println!("\n\x1b[0;32mRHC Bot 管理界面\x1b[0m\n0. 退出\n1. 更改设置\n2. 查看当前设置\n3. 立即更新本地 UBI 缓存\n4. 重置验证数据\n5. 启动服务\n6. 停止服务\n7. 重启服务\n8. 设置开机自启\n9. 关闭开机自启\n10. 添加白名单 TG ID\n11. 删除白名单 TG ID\n12. 查询白名单 TG ID\n13. 删除全部白名单 TG ID\n\n镜像模式: {} | 缓存更新周期: {}", c.image_mode, c.cache_update_interval);
        print!("请选择 [0-13]: ");
        io::stdout().flush()?;
        let mut value = String::new();
        io::stdin().read_line(&mut value)?;
        match value.trim() {
            "0" => return Ok(()),
            "1" => config(data)?,
            "2" => println!("{}", serde_json::to_string_pretty(&c)?),
            "3" => {
                update_cached_image(data, &c, true).await?;
                println!("本地 UBI 缓存已更新");
            }
            "4" => {
                let (_, p) = paths(data);
                if p.exists() {
                    fs::remove_file(p)?;
                }
                println!("验证数据已重置");
            }
            "5" => {
                systemctl("start").await?;
                println!("服务已启动");
            }
            "6" => {
                systemctl("stop").await?;
                println!("服务已停止");
            }
            "7" => {
                systemctl("restart").await?;
                println!("服务已重启");
            }
            "8" => {
                systemctl("enable").await?;
                println!("已设置开机自启");
            }
            "9" => {
                systemctl("disable").await?;
                println!("已关闭开机自启");
            }
            "10" => {
                let tg_id = read_tg_id("请输入要添加的 Telegram 数字 ID: ")?;
                whitelist_add(data, tg_id)?;
                println!("已添加白名单 TG ID: {tg_id}");
            }
            "11" => {
                let tg_id = read_tg_id("请输入要删除的 Telegram 数字 ID: ")?;
                let deleted = whitelist_delete(data, tg_id)?;
                if deleted == 0 {
                    println!("白名单中没有 TG ID: {tg_id}");
                } else {
                    println!("已删除白名单 TG ID: {tg_id}");
                }
            }
            "12" => print_whitelist(data)?,
            "13" => {
                let deleted = whitelist_clear(data)?;
                println!("已删除全部白名单 TG ID，共 {deleted} 个");
            }
            _ => println!("输入无效"),
        }
    }
}
fn read_tg_id(prompt: &str) -> Result<i64> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut s = String::new();
    io::stdin().read_line(&mut s)?;
    s.trim().parse().context("请输入 Telegram 数字 ID")
}
fn print_whitelist(data: &PathBuf) -> Result<()> {
    let ids = whitelist_list(data)?;
    if ids.is_empty() {
        println!("白名单为空");
    } else {
        for id in ids {
            println!("{id}");
        }
    }
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => menu(&cli.data).await,
        Some(action) => match action {
            Action::Run => run(cli.data).await,
            Action::Config => config(&cli.data),
            Action::CacheUpdate => update_cached_image(&cli.data, &load(&cli.data)?, true).await,
            Action::Whitelist { command } => match command {
                WhitelistAction::Add { tg_id } => {
                    whitelist_add(&cli.data, tg_id)?;
                    println!("已添加白名单 TG ID: {tg_id}");
                    Ok(())
                }
                WhitelistAction::Delete { tg_id, all } => {
                    if all {
                        let deleted = whitelist_clear(&cli.data)?;
                        println!("已删除全部白名单 TG ID，共 {deleted} 个");
                    } else if let Some(tg_id) = tg_id {
                        let deleted = whitelist_delete(&cli.data, tg_id)?;
                        if deleted == 0 {
                            println!("白名单中没有 TG ID: {tg_id}");
                        } else {
                            println!("已删除白名单 TG ID: {tg_id}");
                        }
                    } else {
                        bail!("请提供要删除的 Telegram 数字 ID，或使用 --all 删除全部")
                    }
                    Ok(())
                }
                WhitelistAction::List => print_whitelist(&cli.data),
            },
            Action::Start => systemctl("start").await,
            Action::Stop => systemctl("stop").await,
            Action::Restart => systemctl("restart").await,
            Action::Enable => systemctl("enable").await,
            Action::Disable => systemctl("disable").await,
            Action::Reset => {
                let (_, p) = paths(&cli.data);
                if p.exists() {
                    fs::remove_file(p)?
                }
                println!("数据已重置");
                Ok(())
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_supports_fullname_and_newlines() {
        let member = json!({"first_name":"Alice","last_name":"Chen","username":"alice"});
        assert_eq!(
            welcome_text("Hi !\n{fullname}\n\nWelcome", &member),
            "Hi !\nAlice Chen\n\nWelcome"
        );
    }

    #[test]
    fn prompt_config_supports_escaped_newlines() {
        let prompt = r"请输入验证命令。\nEnter the verification command.".replace("\\n", "\n");
        assert_eq!(prompt, "请输入验证命令。\nEnter the verification command.");
    }

    #[test]
    fn accepts_and_normalizes_both_verification_commands() {
        let key = "es9s5n3d-90a3-052d-s1km-s03b6ds7a013";
        let expected = Some((key.to_owned(), "19736027".to_owned()));
        assert_eq!(
            verification_credentials(&format!(
                "rhc connect --activation-key={key} --organization=19736027"
            )),
            expected
        );
        assert_eq!(
            verification_credentials(&format!(
                "subscription-manager register --activationkey={key} --org=19736027"
            )),
            expected
        );
    }

    #[test]
    fn parses_service_management_commands() {
        for command in ["start", "stop", "restart", "enable", "disable"] {
            assert!(Cli::try_parse_from(["rhc-bot", command]).is_ok());
        }
    }

    #[test]
    fn parses_whitelist_commands() {
        assert!(Cli::try_parse_from(["rhc-bot", "whitelist", "add", "123456"]).is_ok());
        assert!(Cli::try_parse_from(["rhc-bot", "whitelist", "delete", "123456"]).is_ok());
        assert!(Cli::try_parse_from(["rhc-bot", "whitelist", "delete", "--all"]).is_ok());
        assert!(Cli::try_parse_from(["rhc-bot", "whitelist", "list"]).is_ok());
    }

    #[test]
    fn manages_whitelist_tg_ids() {
        let data = std::env::temp_dir().join(format!("rhc-bot-test-{}", now()));
        whitelist_add(&data, 123456).unwrap();
        whitelist_add(&data, 987654).unwrap();
        assert!(is_whitelisted(&data, 123456).unwrap());
        assert_eq!(whitelist_list(&data).unwrap(), vec![123456, 987654]);
        assert_eq!(whitelist_delete(&data, 123456).unwrap(), 1);
        assert!(!is_whitelisted(&data, 123456).unwrap());
        assert_eq!(whitelist_clear(&data).unwrap(), 1);
        assert!(whitelist_list(&data).unwrap().is_empty());
        let _ = fs::remove_dir_all(data);
    }
}
