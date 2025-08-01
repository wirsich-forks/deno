// Copyright 2018-2025 the Deno authors. MIT license.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use deno_core::InspectorPostMessageError;
use deno_core::InspectorPostMessageErrorKind;
use deno_core::LocalInspectorSession;
use deno_core::error::CoreError;
use deno_core::futures::StreamExt;
use deno_core::serde_json::json;
use deno_core::serde_json::{self};
use deno_core::url::Url;
use deno_error::JsErrorBox;
use deno_terminal::colors;
use tokio::select;

use crate::cdp;
use crate::module_loader::CliEmitter;
use crate::util::file_watcher::WatcherCommunicator;
use crate::util::file_watcher::WatcherRestartMode;

fn explain(response: &cdp::SetScriptSourceResponse) -> String {
  match response.status {
    cdp::Status::Ok => "OK".to_string(),
    cdp::Status::CompileError => {
      if let Some(details) = &response.exception_details {
        let (message, description) = details.get_message_and_description();
        format!(
          "compile error: {}{}",
          message,
          if description == "undefined" {
            "".to_string()
          } else {
            format!(" - {}", description)
          }
        )
      } else {
        "compile error: No exception details available".to_string()
      }
    }
    cdp::Status::BlockedByActiveGenerator => {
      "blocked by active generator".to_string()
    }
    cdp::Status::BlockedByActiveFunction => {
      "blocked by active function".to_string()
    }
    cdp::Status::BlockedByTopLevelEsModuleChange => {
      "blocked by top-level ES module change".to_string()
    }
  }
}

fn should_retry(status: &cdp::Status) -> bool {
  match status {
    cdp::Status::Ok => false,
    cdp::Status::CompileError => false,
    cdp::Status::BlockedByActiveGenerator => true,
    cdp::Status::BlockedByActiveFunction => true,
    cdp::Status::BlockedByTopLevelEsModuleChange => false,
  }
}

/// This structure is responsible for providing Hot Module Replacement
/// functionality.
///
/// It communicates with V8 inspector over a local session and waits for
/// notifications about changed files from the `FileWatcher`.
///
/// Upon receiving such notification, the runner decides if the changed
/// path should be handled the `FileWatcher` itself (as if we were running
/// in `--watch` mode), or if the path is eligible to be hot replaced in the
/// current program.
///
/// Even if the runner decides that a path will be hot-replaced, the V8 isolate
/// can refuse to perform hot replacement, eg. a top-level variable/function
/// of an ES module cannot be hot-replaced. In such situation the runner will
/// force a full restart of a program by notifying the `FileWatcher`.
pub struct HmrRunner {
  session: LocalInspectorSession,
  watcher_communicator: Arc<WatcherCommunicator>,
  script_ids: HashMap<String, String>,
  emitter: Arc<CliEmitter>,
}

impl HmrRunner {
  pub fn new(
    emitter: Arc<CliEmitter>,
    session: LocalInspectorSession,
    watcher_communicator: Arc<WatcherCommunicator>,
  ) -> Self {
    Self {
      session,
      emitter,
      watcher_communicator,
      script_ids: HashMap::new(),
    }
  }

  // TODO(bartlomieju): this code is duplicated in `cli/tools/coverage/mod.rs`
  pub async fn start(&mut self) -> Result<(), InspectorPostMessageError> {
    self.enable_debugger().await
  }

  // TODO(bartlomieju): this code is duplicated in `cli/tools/coverage/mod.rs`
  pub async fn stop(&mut self) -> Result<(), InspectorPostMessageError> {
    self
      .watcher_communicator
      .change_restart_mode(WatcherRestartMode::Automatic);
    self.disable_debugger().await
  }

  pub async fn run(&mut self) -> Result<(), CoreError> {
    self
      .watcher_communicator
      .change_restart_mode(WatcherRestartMode::Manual);
    let mut session_rx = self.session.take_notification_rx();
    loop {
      select! {
        biased;
        Some(notification) = session_rx.next() => {
          let notification = serde_json::from_value::<cdp::Notification>(notification).map_err(JsErrorBox::from_err)?;
          if notification.method == "Runtime.exceptionThrown" {
            let exception_thrown = serde_json::from_value::<cdp::ExceptionThrown>(notification.params).map_err(JsErrorBox::from_err)?;
            let (message, description) = exception_thrown.exception_details.get_message_and_description();
            break Err(JsErrorBox::generic(format!("{} {}", message, description)).into());
          } else if notification.method == "Debugger.scriptParsed" {
            let params = serde_json::from_value::<cdp::ScriptParsed>(notification.params).map_err(JsErrorBox::from_err)?;
            if params.url.starts_with("file://") {
              let file_url = Url::parse(&params.url).unwrap();
              let file_path = file_url.to_file_path().unwrap();
              if let Ok(canonicalized_file_path) = file_path.canonicalize() {
                let canonicalized_file_url = Url::from_file_path(canonicalized_file_path).unwrap();
                self.script_ids.insert(canonicalized_file_url.into(), params.script_id);
              }
            }
          }
        }
        changed_paths = self.watcher_communicator.watch_for_changed_paths() => {
          let changed_paths = changed_paths.map_err(JsErrorBox::from_err)?;

          let Some(changed_paths) = changed_paths else {
            let _ = self.watcher_communicator.force_restart();
            continue;
          };

          let filtered_paths: Vec<PathBuf> = changed_paths.into_iter().filter(|p| p.extension().is_some_and(|ext| {
            let ext_str = ext.to_str().unwrap();
            matches!(ext_str, "js" | "ts" | "jsx" | "tsx")
          })).collect();

          // If after filtering there are no paths it means it's either a file
          // we can't HMR or an external file that was passed explicitly to
          // `--watch-hmr=<file>` path.
          if filtered_paths.is_empty() {
            let _ = self.watcher_communicator.force_restart();
            continue;
          }

          for path in filtered_paths {
            let Some(path_str) = path.to_str() else {
              let _ = self.watcher_communicator.force_restart();
              continue;
            };
            let Ok(module_url) = Url::from_file_path(path_str) else {
              let _ = self.watcher_communicator.force_restart();
              continue;
            };

            let Some(id) = self.script_ids.get(module_url.as_str()).cloned() else {
              let _ = self.watcher_communicator.force_restart();
              continue;
            };

            let source_code = tokio::fs::read_to_string(deno_path_util::url_to_file_path(&module_url).unwrap()).await?;
            let source_code = self.emitter.emit_for_hmr(
              &module_url,
              source_code,
            )?;

            let mut tries = 1;
            loop {
              let result = self.set_script_source(&id, source_code.as_str()).await?;

              if matches!(result.status, cdp::Status::Ok) {
                self.dispatch_hmr_event(module_url.as_str()).await?;
                self.watcher_communicator.print(format!("Replaced changed module {}", module_url.as_str()));
                break;
              }

              self.watcher_communicator.print(format!("Failed to reload module {}: {}.", module_url, colors::gray(&explain(&result))));
              if should_retry(&result.status) && tries <= 2 {
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
              }

              let _ = self.watcher_communicator.force_restart();
              break;
            }
          }
        }
        _ = self.session.receive_from_v8_session() => {}
      }
    }
  }

  // TODO(bartlomieju): this code is duplicated in `cli/tools/coverage/mod.rs`
  async fn enable_debugger(&mut self) -> Result<(), InspectorPostMessageError> {
    self
      .session
      .post_message::<()>("Debugger.enable", None)
      .await?;
    self
      .session
      .post_message::<()>("Runtime.enable", None)
      .await?;
    Ok(())
  }

  // TODO(bartlomieju): this code is duplicated in `cli/tools/coverage/mod.rs`
  async fn disable_debugger(
    &mut self,
  ) -> Result<(), InspectorPostMessageError> {
    self
      .session
      .post_message::<()>("Debugger.disable", None)
      .await?;
    self
      .session
      .post_message::<()>("Runtime.disable", None)
      .await?;
    Ok(())
  }

  async fn set_script_source(
    &mut self,
    script_id: &str,
    source: &str,
  ) -> Result<cdp::SetScriptSourceResponse, InspectorPostMessageError> {
    let result = self
      .session
      .post_message(
        "Debugger.setScriptSource",
        Some(json!({
          "scriptId": script_id,
          "scriptSource": source,
          "allowTopFrameEditing": true,
        })),
      )
      .await?;

    serde_json::from_value::<cdp::SetScriptSourceResponse>(result).map_err(
      |e| {
        InspectorPostMessageErrorKind::JsBox(JsErrorBox::from_err(e)).into_box()
      },
    )
  }

  async fn dispatch_hmr_event(
    &mut self,
    script_id: &str,
  ) -> Result<(), InspectorPostMessageError> {
    let expr = format!(
      "dispatchEvent(new CustomEvent(\"hmr\", {{ detail: {{ path: \"{}\" }} }}));",
      script_id
    );

    let _result = self
      .session
      .post_message(
        "Runtime.evaluate",
        Some(json!({
          "expression": expr,
          "contextId": Some(1),
        })),
      )
      .await?;

    Ok(())
  }
}
