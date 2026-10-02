use agent_chat_protocol::ConversationEvent;
use ts_rs::TS;

fn main() {
    let out =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/chat/src/generated");
    // Consumer の TS_RS_EXPORT_DIR に影響されず、この package 内を正本にする。
    ConversationEvent::export_all(&ts_rs::Config::new().with_out_dir(out))
        .expect("共有チャット型の生成に失敗");
}
