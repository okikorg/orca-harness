use std::path::Path;

use tokio::sync::mpsc;

use crate::msg::UiMsg;

pub(super) async fn test(path: &Path, ui: &mpsc::UnboundedSender<UiMsg>) {
    for line in crate::plugin::test_report(path).await {
        let _ = ui.send(UiMsg::Notice(line));
    }
}
