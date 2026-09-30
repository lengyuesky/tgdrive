//! Bot 不能调用 messages.getHistory/search。用频道差分取得消息上界，再按真实
//! 消息号分批调用 channels.getMessages；无需旧会话、手机号或临时发送消息。
use super::*;
use crate::telegram::{TgHistoryDocument, TgHistoryPage};

// 每页只读一批，交由恢复管理器判断是否已找到 5 份，避免继续读取旧消息。
const PAGE_IDS: i32 = 100;
const BATCH_IDS: i32 = 100;

impl MtprotoClient {
    pub async fn backup_history_page(
        &self,
        chat_id: &str,
        before: Option<i32>,
    ) -> AppResult<TgHistoryPage> {
        let state = self.state().await?;
        let channel = resolve_channel(&state.client, chat_id).await?;
        let high = match before {
            Some(before) if before > 0 => before - 1,
            Some(_) => return Err(AppError::BadRequest("频道探测游标无效".into())),
            None => channel_top(&state.client, &channel).await?,
        };
        let low = (high - PAGE_IDS + 1).max(1);
        let mut documents = Vec::new();
        let mut end = high;
        while end >= low {
            let start = (end - BATCH_IDS + 1).max(low);
            let result = state
                .client
                .invoke(&tl::functions::channels::GetMessages {
                    channel: channel.clone(),
                    id: (start..=end)
                        .map(|id| tl::types::InputMessageId { id }.into())
                        .collect(),
                })
                .await
                .map_err(|e| mtproto_error("读取频道历史消息失败", e))?;
            let messages = match result {
                tl::enums::messages::Messages::Messages(v) => v.messages,
                tl::enums::messages::Messages::Slice(v) => v.messages,
                tl::enums::messages::Messages::ChannelMessages(v) => v.messages,
                _ => {
                    return Err(AppError::Telegram(
                        "频道历史未返回消息，无法完成探测".into(),
                    ));
                }
            };
            for message in messages {
                let tl::enums::Message::Message(message) = message else {
                    continue;
                };
                // 恢复时的临时转发、其他频道转入的文件不作为原始备份。
                if message.id < start || message.id > end || message.fwd_from.is_some() {
                    continue;
                }
                let Some(tl::enums::MessageMedia::Document(media)) = message.media else {
                    continue;
                };
                let Some(tl::enums::Document::Document(document)) = media.document else {
                    continue;
                };
                for attribute in document.attributes {
                    if let tl::enums::DocumentAttribute::Filename(name) = attribute
                        && name.file_name.starts_with("tgdrive-backup-")
                    {
                        documents.push(TgHistoryDocument {
                            message_id: message.id,
                            file_name: name.file_name,
                        });
                        break;
                    }
                }
            }
            end = start - 1;
            if end >= low {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        Ok(TgHistoryPage {
            documents,
            next_before: (low > 1).then_some(low),
            scanned: (high - low + 1).max(0) as usize,
        })
    }
}

async fn resolve_channel(client: &Client, chat_id: &str) -> AppResult<tl::enums::InputChannel> {
    let id = if let Some(username) = chat_id.strip_prefix('@') {
        let tl::enums::contacts::ResolvedPeer::Peer(peer) = client
            .invoke(&tl::functions::contacts::ResolveUsername {
                username: username.into(),
                referer: None,
            })
            .await
            .map_err(|e| mtproto_error("解析备份频道失败", e))?;
        let tl::enums::Peer::Channel(channel) = peer.peer else {
            return Err(AppError::BadRequest("备份存储目标必须是频道".into()));
        };
        channel.channel_id
    } else {
        chat_id
            .strip_prefix("-100")
            .and_then(|id| id.parse::<i64>().ok())
            .filter(|id| *id > 0)
            .ok_or_else(|| {
                AppError::BadRequest("频道探测需要 -100 开头的频道 ID 或 @频道用户名".into())
            })?
    };
    // Telegram 明确允许 Bot 在没有缓存 access_hash 时使用 0。
    // https://core.telegram.org/api/peers#access-hash
    let channel = tl::types::InputChannel {
        channel_id: id,
        access_hash: 0,
    };
    // 先取完整频道信息，既验证访问权限，也取得后续差分接口所需的完整 hash。
    let result = client
        .invoke(&tl::functions::channels::GetChannels {
            id: vec![channel.clone().into()],
        })
        .await
        .map_err(|e| mtproto_error("读取备份频道信息失败", e))?;
    let chats = match result {
        tl::enums::messages::Chats::Chats(v) => v.chats,
        tl::enums::messages::Chats::Slice(v) => v.chats,
    };
    for chat in chats {
        if let tl::enums::Chat::Channel(chat) = chat
            && chat.id == id
        {
            return Ok(tl::types::InputChannel {
                channel_id: id,
                access_hash: chat.access_hash.unwrap_or(0),
            }
            .into());
        }
    }
    Err(AppError::BadRequest(
        "无法访问备份频道，请确认 Bot 已加入频道并设为管理员".into(),
    ))
}

fn message_id(message: &tl::enums::Message) -> i32 {
    match message {
        tl::enums::Message::Message(v) => v.id,
        tl::enums::Message::Service(v) => v.id,
        tl::enums::Message::Empty(v) => v.id,
    }
}

async fn channel_top(client: &Client, channel: &tl::enums::InputChannel) -> AppResult<i32> {
    let mut pts = 1;
    let mut top = 0;
    for _ in 0..100 {
        let difference = client
            .invoke(&tl::functions::updates::GetChannelDifference {
                force: true,
                channel: channel.clone(),
                filter: tl::enums::ChannelMessagesFilter::Empty,
                pts,
                limit: 100,
            })
            .await
            .map_err(|e| mtproto_error("探测频道最新消息失败", e))?;
        match difference {
            tl::enums::updates::ChannelDifference::TooLong(v) => {
                let tl::enums::Dialog::Dialog(dialog) = v.dialog else {
                    return Err(AppError::Telegram("备份频道未返回有效消息上界".into()));
                };
                return Ok(dialog.top_message.max(0));
            }
            tl::enums::updates::ChannelDifference::Difference(v) => {
                top = top.max(v.new_messages.iter().map(message_id).max().unwrap_or(0));
                if v.r#final {
                    return Ok(top);
                }
                if v.pts <= pts {
                    break;
                }
                pts = v.pts;
            }
            tl::enums::updates::ChannelDifference::Empty(_) => return Ok(top),
        }
    }
    Err(AppError::Telegram("频道更新过多，请稍后重新探测".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手动运行：只读真实频道，在临时数据库中登录，不接触运行中的数据库。
    #[tokio::test]
    #[ignore = "需要显式提供真实 Telegram 环境变量和网络"]
    async fn live_channel_discovery() {
        let cfg = crate::config::Config::from_env().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = Db::open(temp.path().join("test.db").to_str().unwrap())
            .await
            .unwrap();
        let config =
            MtprotoConfig::from_values(db.clone(), &cfg.telegram_api_id, &cfg.telegram_api_hash)
                .unwrap()
                .unwrap();
        let network = Arc::new(ProxyClient::new(cfg.proxy.clone()).unwrap());
        let client = MtprotoClient::new(config, 1, &cfg.bot_token, network);
        let page = tokio::time::timeout(
            std::time::Duration::from_secs(90),
            client.backup_history_page(&cfg.chat_id, None),
        )
        .await
        .unwrap()
        .unwrap();
        println!(
            "已读取 {} 个消息位置，发现 {} 个备份文档，是否还有更早消息：{}",
            page.scanned,
            page.documents.len(),
            page.next_before.is_some()
        );
        db.close().await;
    }
}
