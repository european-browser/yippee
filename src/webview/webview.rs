use arboard::Clipboard;
use base::id::WebViewId;
use compositing_traits::ConstellationMsg;
use crossbeam_channel::Sender;
use embedder_traits::{
    AllowOrDeny, ContextMenuResult, EmbedderMsg, LoadStatus, PromptDefinition, PromptResult,
    TraversalDirection,
};
use ipc_channel::ipc;
use script_traits::webdriver_msg::{WebDriverJSResult, WebDriverScriptCommand};
use servo_url::ServoUrl;
use url::Url;
use versoview_messages::ToControllerMessage;
use webrender_api::units::DeviceIntRect;

use crate::{
    compositor::IOCompositor,
    tab::{TabActivateRequest, TabCloseRequest, TabCreateResponse},
    verso::send_to_constellation,
    webview::prompt::{HttpBasicAuthInputResult, PromptDialog, PromptInputResult, PromptSender},
    window::Window,
};

#[cfg(linux)]
use crate::webview::context_menu::ContextMenuUIResponse;

/// A web view is an area to display web browsing context. It's what user will treat as a "web page".
#[derive(Debug, Clone)]
pub struct WebView {
    /// Webview ID
    pub webview_id: WebViewId,
    /// The position and size of the webview.
    pub rect: DeviceIntRect,
}

impl WebView {
    /// Create a web view.
    pub fn new(webview_id: WebViewId, rect: DeviceIntRect) -> Self {
        Self { webview_id, rect }
    }

    /// Set the webview size.
    pub fn set_size(&mut self, rect: DeviceIntRect) {
        self.rect = rect;
    }
}

/// A panel is a special web view that focus on controlling states around window.
/// It could be treated as the control panel or navigation bar of the window depending on usages.
///
/// At the moment, following Web API is supported:
/// - Close window: `window.close()`
/// - Navigate to previous page: `window.prompt('PREV')`
/// - Navigate to next page: `window.prompt('FORWARD')`
/// - Refresh the page: `window.prompt('REFRESH')`
/// - Minimize the window: `window.prompt('MINIMIZE')`
/// - Maximize the window: `window.prompt('MAXIMIZE')`
/// - Navigate to a specific URL: `window.prompt('NAVIGATE_TO:${url}')`
pub struct Panel {
    /// The panel's webview
    pub(crate) webview: WebView,
    /// The URL to load when the panel gets loaded
    pub(crate) initial_url: servo_url::ServoUrl,
}

impl Window {
    /// Handle servo messages with corresponding web view ID.
    pub fn handle_servo_messages_with_webview(
        &mut self,
        webview_id: WebViewId,
        message: EmbedderMsg,
        sender: &Sender<ConstellationMsg>,
        to_controller_sender: &Option<ipc::IpcSender<ToControllerMessage>>,
        clipboard: Option<&mut Clipboard>,
        compositor: &mut IOCompositor,
    ) {
        log::trace!("Verso WebView {webview_id:?} is handling Embedder message: {message:?}",);
        match message {
            EmbedderMsg::WebViewOpened(_) | EmbedderMsg::WebViewClosed(_) => {
                // Most WebView messages are ignored because it's done by compositor.
                log::trace!("Verso WebView {webview_id:?} ignores this message: {message:?}")
            }
            EmbedderMsg::WebViewBlurred => {
                self.focused_webview_id = None;
                self.close_context_menu(sender);
            }
            EmbedderMsg::WebViewFocused(w) => {
                self.focused_webview_id = Some(webview_id);
                self.close_context_menu(sender);
                log::debug!(
                    "Verso Window {:?}'s webview {} has loaded completely.",
                    self.id(),
                    w
                );
            }
            EmbedderMsg::NotifyLoadStatusChanged(_webview_id, status) => match status {
                LoadStatus::Started => {
                    if let Some(init_script) = &self.init_script {
                        let _ = execute_script(sender, &webview_id, init_script);
                    }
                }
                LoadStatus::Complete => {
                    self.window.request_redraw();
                    send_to_constellation(sender, ConstellationMsg::FocusWebView(webview_id));
                }
                _ => {
                    log::trace!(
                            "Verso WebView {webview_id:?} ignores NotifyLoadStatusChanged status: {status:?}"
                        );
                }
            },
            EmbedderMsg::ChangePageTitle(_webview_id, title) => {
                if let Some(panel) = self.panel.as_ref() {
                    let title = if let Some(title) = title {
                        format!("'{title}'")
                    } else {
                        "null".to_string()
                    };

                    let script = format!(
                        "window.navbar.setTabTitle('{}', {})",
                        serde_json::to_string(&webview_id).unwrap(),
                        title.as_str()
                    );
                    let _ = execute_script(sender, &panel.webview.webview_id, script);
                }
            }
            EmbedderMsg::AllowNavigationRequest(_webview_id, id, url) => {
                if let Some(to_controller_sender) = to_controller_sender {
                    if self.event_listeners.on_navigation_starting {
                        if let Err(error) =
                            to_controller_sender.send(ToControllerMessage::OnNavigationStarting(
                                bincode::serialize(&id).unwrap(),
                                url.into_url(),
                            ))
                        {
                            log::error!("Verso failed to send AllowNavigationRequest to controller: {error}")
                        } else {
                            // We will handle a ToVersoMessage::OnNavigationStartingResponse
                            // and send ConstellationMsg::AllowNavigationResponse there if the call succeed
                            return;
                        }
                    }
                }
                send_to_constellation(sender, ConstellationMsg::AllowNavigationResponse(id, true));
            }
            EmbedderMsg::WebResourceRequested(_webview_id, request, sender) => {
                if let Some(to_controller_sender) = to_controller_sender {
                    if let Some(request_map) = &mut self.event_listeners.on_web_resource_requested {
                        let id = uuid::Uuid::new_v4();
                        let mut builder = http::request::Builder::new()
                            .uri(request.url.as_str())
                            .method(request.method);
                        for (key, value) in request.headers.iter() {
                            builder = builder.header(key, value);
                        }
                        match to_controller_sender.send(
                            ToControllerMessage::OnWebResourceRequested(
                                versoview_messages::WebResourceRequest {
                                    id,
                                    // TODO: Actually send the body
                                    request: builder.body(Vec::new()).unwrap(),
                                },
                            ),
                        ) {
                            Ok(_) => {
                                request_map.insert(id, (request.url, sender));
                                // We will handle a ToVersoMessage::WebResourceRequestResponse
                                // and send the response through this sender there if the call succeed
                                return;
                            }
                            Err(error) => {
                                log::error!("Verso failed to send WebResourceRequested to controller: {error}")
                            }
                        }
                    }
                }
            }
            EmbedderMsg::GetClipboardText(_webview_id, sender) => {
                let text = clipboard
                    .map(|c| {
                        c.get_text().unwrap_or_else(|e| {
                            log::warn!(
                                "Verso WebView {webview_id:?} failed to get clipboard text: {}",
                                e
                            );
                            String::new()
                        })
                    })
                    .unwrap_or_default();
                if let Err(e) = sender.send(Ok(text)) {
                    log::warn!(
                        "Verso WebView {webview_id:?} failed to send clipboard text: {}",
                        e
                    );
                }
            }
            EmbedderMsg::SetClipboardText(_webview_id, text) => {
                if let Some(c) = clipboard {
                    if let Err(e) = c.set_text(text) {
                        log::warn!(
                            "Verso WebView {webview_id:?} failed to set clipboard text: {}",
                            e
                        );
                    }
                }
            }
            EmbedderMsg::HistoryChanged(_webview_id, list, index) => {
                self.close_prompt_dialog(webview_id);
                compositor.send_root_pipeline_display_list(self);

                self.tab_manager
                    .set_history(webview_id, list.clone(), index);
                let url = list.get(index).unwrap();
                if let Some(panel) = self.panel.as_ref() {
                    let _ = execute_script(
                        sender,
                        &panel.webview.webview_id,
                        format!("window.navbar.setNavbarUrl('{}')", url.as_str()),
                    );
                }
            }
            EmbedderMsg::ShowContextMenu(_webview_id, servo_sender, _title, _options) => {
                #[cfg(linux)]
                if self.context_menu.is_none() {
                    self.context_menu = Some(self.show_context_menu(sender, servo_sender));
                } else {
                    let _ = servo_sender.send(ContextMenuResult::Ignored);
                }
                #[cfg(any(target_os = "windows", target_os = "macos"))]
                {
                    let context_menu = self.show_context_menu(servo_sender);
                    // FIXME: there's chance to lose the event since the channel is async.
                    if let Ok(event) = self.menu_event_receiver.try_recv() {
                        self.handle_context_menu_event(context_menu, sender, event);
                    }
                }
            }
            EmbedderMsg::Prompt(_webview_id, prompt_type, _origin) => {
                if let Some(tab) = self.tab_manager.tab(webview_id) {
                    let mut prompt = PromptDialog::new();
                    let rect = tab.webview().rect;
                    match prompt_type {
                        PromptDefinition::Alert(message, prompt_sender) => {
                            prompt.alert(sender, rect, message, prompt_sender);
                        }
                        PromptDefinition::OkCancel(message, prompt_sender) => {
                            prompt.ok_cancel(sender, rect, message, prompt_sender);
                        }
                        PromptDefinition::Input(message, default_value, prompt_sender) => {
                            prompt.input(sender, rect, message, Some(default_value), prompt_sender);
                        }
                    }

                    // save prompt in window to keep prompt_sender alive
                    // so that we can send the result back to the prompt after user clicked the button
                    self.tab_manager.set_prompt(webview_id, prompt);
                } else {
                    log::error!("Failed to get WebView {webview_id:?} in this window.");
                }
            }
            EmbedderMsg::PromptPermission(_webview_id, feature, prompt_sender) => {
                if let Some(tab) = self.tab_manager.tab(webview_id) {
                    let message = format!(
                        "This website would like to request permission for {:?}.",
                        feature
                    );

                    let mut prompt = PromptDialog::new();
                    prompt.allow_deny(
                        sender,
                        tab.webview().rect,
                        message,
                        PromptSender::AllowDenySender(prompt_sender),
                    );
                    self.tab_manager.set_prompt(webview_id, prompt);
                } else {
                    log::error!("Failed to get WebView {webview_id:?} in this window.");
                }
            }
            EmbedderMsg::RequestAuthentication(_webview_id, _url, _proxy, response_sender) => {
                if let Some(tab) = self.tab_manager.tab(webview_id) {
                    let mut prompt = PromptDialog::new();
                    let rect = tab.webview().rect;
                    prompt.http_basic_auth(sender, rect, response_sender);
                    self.tab_manager.set_prompt(webview_id, prompt);
                } else {
                    log::error!("Failed to get WebView {webview_id:?} in this window.");
                }
            }
            EmbedderMsg::ShowIME(_webview_id, input_method_type, text, multiline, position) => {
                self.show_ime(input_method_type, text, multiline, position);
            }
            EmbedderMsg::HideIME(_webview_id) => {
                self.hide_ime();
            }
            e => {
                log::trace!("Verso WebView isn't supporting this message yet: {e:?}")
            }
        }
    }

    /// Handle servo messages with main panel. Return true it requests a new window.
    pub fn handle_servo_messages_with_panel(
        &mut self,
        panel_id: WebViewId,
        message: EmbedderMsg,
        sender: &Sender<ConstellationMsg>,
        clipboard: Option<&mut Clipboard>,
        compositor: &mut IOCompositor,
    ) -> bool {
        log::trace!("Verso Panel {panel_id:?} is handling Embedder message: {message:?}",);
        match message {
            EmbedderMsg::WebViewOpened(_) | EmbedderMsg::WebViewClosed(_) => {
                // Most WebView messages are ignored because it's done by compositor.
                log::trace!("Verso Panel ignores this message: {message:?}")
            }
            EmbedderMsg::WebViewBlurred => {
                self.focused_webview_id = None;
            }
            EmbedderMsg::WebViewFocused(webview_id) => {
                self.focused_webview_id = Some(webview_id);
                self.close_context_menu(sender);
                log::debug!(
                    "Verso Window {:?}'s panel {} has loaded completely.",
                    self.id(),
                    webview_id
                );
            }
            EmbedderMsg::NotifyLoadStatusChanged(_webview_id, status) => {
                if status == LoadStatus::Complete {
                    self.window.request_redraw();
                    send_to_constellation(sender, ConstellationMsg::FocusWebView(panel_id));

                    self.create_tab(sender, self.panel.as_ref().unwrap().initial_url.clone());
                } else {
                    log::trace!("Verso Panel ignores NotifyLoadStatusChanged status: {status:?}");
                }
            }
            EmbedderMsg::AllowNavigationRequest(_webview_id, id, _url) => {
                // The panel shouldn't navigate to other pages.
                send_to_constellation(sender, ConstellationMsg::AllowNavigationResponse(id, false));
            }
            EmbedderMsg::HistoryChanged(..) | EmbedderMsg::ChangePageTitle(..) => {
                log::trace!("Verso Panel ignores this message: {message:?}")
            }
            EmbedderMsg::Prompt(_webview_id, definition, _origin) => {
                match definition {
                    PromptDefinition::Input(msg, _, prompt_sender) => {
                        /* Tab */
                        if msg.starts_with("CLOSE_TAB:") {
                            let request_str = msg.strip_prefix("CLOSE_TAB:").unwrap();
                            let request: TabCloseRequest = serde_json::from_str(request_str)
                                .expect("Failed to parse TabCloseRequest");

                            // close the tab
                            if self.tab_manager.tab(request.id).is_some() {
                                send_to_constellation(
                                    sender,
                                    ConstellationMsg::CloseWebView(request.id),
                                );
                            }

                            let _ = prompt_sender.send(None);
                            return false;
                        } else if msg.starts_with("ACTIVATE_TAB:") {
                            let request_str = msg.strip_prefix("ACTIVATE_TAB:").unwrap();
                            let request: TabActivateRequest = serde_json::from_str(request_str)
                                .expect("Failed to parse TabActivateRequest");

                            let tab_id = request.id;

                            // FIXME: set dirty flag, and only resize when flag is set
                            self.activate_tab(compositor, tab_id, self.tab_manager.count() > 1);

                            let _ = prompt_sender.send(None);
                            return false;
                        } else if msg == "NEW_TAB" {
                            let webview_id = WebViewId::new();
                            let size = self.size();
                            let rect = DeviceIntRect::from_size(size);
                            let content_size = self.get_content_size(rect, true);
                            let webview = WebView::new(webview_id, content_size);

                            self.tab_manager.append_tab(webview, true);

                            send_to_constellation(
                                sender,
                                ConstellationMsg::NewWebView(
                                    ServoUrl::parse("https://example.com").unwrap(),
                                    webview_id,
                                ),
                            );
                            let result = TabCreateResponse {
                                success: true,
                                id: webview_id,
                            };
                            let _ = prompt_sender.send(Some(result.to_json()));
                            return false;
                        }

                        let _ = prompt_sender.send(None);

                        /* Window */
                        match msg.as_str() {
                            "NEW_WINDOW" => {
                                let _ = prompt_sender.send(None);
                                return true;
                            }
                            "MINIMIZE" => {
                                self.window.set_minimized(true);
                                return false;
                            }
                            "MAXIMIZE" | "DBCLICK_PANEL" => {
                                let is_maximized = self.window.is_maximized();
                                self.window.set_maximized(!is_maximized);
                                return false;
                            }
                            "DRAG_WINDOW" => {
                                let _ = self.window.drag_window();
                                return false;
                            }
                            _ => {}
                        }

                        /* Main WebView */
                        if let Some(tab) = self.tab_manager.current_tab() {
                            let id = tab.id();
                            if msg.starts_with("NAVIGATE_TO:") {
                                let unparsed_url = msg.strip_prefix("NAVIGATE_TO:").unwrap();
                                let url = match Url::parse(unparsed_url) {
                                    Ok(url_parsed) => url_parsed,
                                    Err(e) => {
                                        if e == url::ParseError::RelativeUrlWithoutBase {
                                            Url::parse(&format!("https://{}", unparsed_url))
                                                .unwrap()
                                        } else {
                                            panic!("Verso Panel failed to parse URL: {}", e);
                                        }
                                    }
                                };

                                send_to_constellation(
                                    sender,
                                    ConstellationMsg::LoadUrl(id, ServoUrl::from_url(url)),
                                );
                            } else {
                                match msg.as_str() {
                                    "PREV" => {
                                        send_to_constellation(
                                            sender,
                                            ConstellationMsg::TraverseHistory(
                                                id,
                                                TraversalDirection::Back(1),
                                            ),
                                        );
                                        // TODO Set EmbedderMsg::Status to None
                                    }
                                    "FORWARD" => {
                                        send_to_constellation(
                                            sender,
                                            ConstellationMsg::TraverseHistory(
                                                id,
                                                TraversalDirection::Forward(1),
                                            ),
                                        );
                                        // TODO Set EmbedderMsg::Status to None
                                    }
                                    "REFRESH" => {
                                        send_to_constellation(sender, ConstellationMsg::Reload(id));
                                    }
                                    e => log::trace!(
                                        "Verso Panel isn't supporting this prompt message yet: {e}"
                                    ),
                                }
                            }
                        }
                    }
                    _ => log::trace!("Verso Panel isn't supporting this prompt yet"),
                }
            }
            EmbedderMsg::GetClipboardText(_webview_id, sender) => {
                let text = clipboard
                    .map(|c| {
                        c.get_text().unwrap_or_else(|e| {
                            log::warn!("Verso Panel failed to get clipboard text: {}", e);
                            String::new()
                        })
                    })
                    .unwrap_or_default();
                if let Err(e) = sender.send(Ok(text)) {
                    log::warn!("Verso Panel failed to send clipboard text: {}", e);
                }
            }
            EmbedderMsg::SetClipboardText(_webview_id, text) => {
                if let Some(c) = clipboard {
                    if let Err(e) = c.set_text(text) {
                        log::warn!("Verso Panel failed to set clipboard text: {}", e);
                    }
                }
            }
            EmbedderMsg::ShowContextMenu(_, servo_sender, _, _) => {
                #[cfg(linux)]
                if self.context_menu.is_none() {
                    self.context_menu = Some(self.show_context_menu(sender, servo_sender));
                } else {
                    let _ = servo_sender.send(ContextMenuResult::Ignored);
                }
                #[cfg(any(target_os = "windows", target_os = "macos"))]
                {
                    let context_menu = self.show_context_menu(servo_sender);
                    // FIXME: there's chance to lose the event since the channel is async.
                    if let Ok(event) = self.menu_event_receiver.try_recv() {
                        self.handle_context_menu_event(context_menu, sender, event);
                    }
                }
            }
            e => {
                log::trace!("Verso Panel isn't supporting this message yet: {e:?}")
            }
        }
        false
    }

    /// Handle servo messages with main panel. Return true it requests a new window.
    #[cfg(linux)]
    pub fn handle_servo_messages_with_context_menu(
        &mut self,
        webview_id: WebViewId,
        message: EmbedderMsg,
        sender: &Sender<ConstellationMsg>,
        _clipboard: Option<&mut Clipboard>,
        _compositor: &mut IOCompositor,
    ) -> bool {
        log::trace!("Verso Context Menu {webview_id:?} is handling Embedder message: {message:?}",);
        match message {
            EmbedderMsg::WebViewBlurred => {
                self.focused_webview_id = None;
            }
            EmbedderMsg::WebViewFocused(webview_id) => {
                self.focused_webview_id = Some(webview_id);
            }
            EmbedderMsg::Prompt(_webview_id, definition, _origin) => match definition {
                PromptDefinition::Input(msg, _, prompt_sender) => {
                    let _ = prompt_sender.send(None);
                    if msg.starts_with("CONTEXT_MENU:") {
                        let json_str_msg = msg.strip_prefix("CONTEXT_MENU:").unwrap();
                        let result =
                            serde_json::from_str::<ContextMenuUIResponse>(json_str_msg).unwrap();

                        self.handle_context_menu_event(sender, result);
                    }
                }
                _ => log::trace!("Verso context menu isn't supporting this prompt yet"),
            },
            EmbedderMsg::ShowContextMenu(_webview_id, servo_sender, _title, _options) => {
                if self.context_menu.is_none() {
                    self.context_menu = Some(self.show_context_menu(sender, servo_sender));
                } else {
                    let _ = servo_sender.send(ContextMenuResult::Ignored);
                }
            }
            e => {
                log::trace!("Verso context menu isn't supporting this message yet: {e:?}")
            }
        }
        false
    }

    /// Handle servo messages with prompt. Return true it requests a new window.
    pub fn handle_servo_messages_with_prompt(
        &mut self,
        webview_id: WebViewId,
        message: EmbedderMsg,
        _sender: &Sender<ConstellationMsg>,
        _clipboard: Option<&mut Clipboard>,
        _compositor: &mut IOCompositor,
    ) -> bool {
        log::trace!("Verso Prompt {webview_id:?} is handling Embedder message: {message:?}",);
        match message {
            EmbedderMsg::WebViewBlurred => {
                self.focused_webview_id = None;
            }
            EmbedderMsg::WebViewFocused(webview_id) => {
                self.focused_webview_id = Some(webview_id);
            }
            EmbedderMsg::Prompt(_webview_id, prompt, _origin) => match prompt {
                PromptDefinition::Alert(msg, dummy_sender) => {
                    let _ = dummy_sender.send(());

                    let Some(prompt) = self.tab_manager.prompt_by_prompt_id(webview_id) else {
                        log::error!("Prompt not found. WebView: {webview_id:?}");
                        return false;
                    };

                    let servo_sender = prompt.sender().unwrap();
                    match servo_sender {
                        PromptSender::AlertSender(sender) => {
                            let _ = sender.send(());
                        }
                        PromptSender::ConfirmSender(sender) => {
                            let result: PromptResult = match msg.as_str() {
                                "ok" => PromptResult::Primary,
                                "cancel" => PromptResult::Secondary,
                                _ => {
                                    log::error!("Invalid prompt action: {msg}");
                                    PromptResult::Dismissed
                                }
                            };
                            let _ = sender.send(result);
                        }
                        PromptSender::InputSender(sender) => {
                            if let Ok(PromptInputResult { action, value }) =
                                serde_json::from_str::<PromptInputResult>(&msg)
                            {
                                match action.as_str() {
                                    "ok" => {
                                        let _ = sender.send(Some(value));
                                    }
                                    "cancel" => {
                                        let _ = sender.send(None);
                                    }
                                    _ => {
                                        log::error!("Invalid prompt action: {msg}");
                                        let _ = sender.send(None);
                                    }
                                }
                            } else {
                                log::error!("Invalid prompt action: {msg}");
                                let _ = sender.send(None);
                            }
                        }
                        PromptSender::AllowDenySender(sender) => {
                            let result: AllowOrDeny = match msg.as_str() {
                                "allow" => AllowOrDeny::Allow,
                                "deny" => AllowOrDeny::Deny,
                                _ => {
                                    log::error!("Invalid prompt action: {msg}");
                                    AllowOrDeny::Deny
                                }
                            };
                            let _ = sender.send(result);
                        }
                        PromptSender::HttpBasicAuthSender(sender) => {
                            if let Ok(HttpBasicAuthInputResult { action, auth }) =
                                serde_json::from_str::<HttpBasicAuthInputResult>(&msg)
                            {
                                match action.as_str() {
                                    "signin" => {
                                        let _ = sender.send(Some(auth));
                                    }
                                    "cancel" => {
                                        let _ = sender.send(None);
                                    }
                                    _ => {
                                        let _ = sender.send(None);
                                    }
                                };
                            } else {
                                log::error!("Invalid prompt action: {msg}");
                                let _ = sender.send(None);
                            }
                        }
                    }
                }
                _ => {
                    log::trace!("Unsupported prompt type");
                }
            },
            EmbedderMsg::ShowContextMenu(_, sender, _, _) => {
                let _ = sender.send(ContextMenuResult::Ignored);
            }
            e => {
                log::trace!("Verso Dialog isn't supporting this message yet: {e:?}")
            }
        }
        false
    }
}

/// Blocking execute a script on this webview
pub fn execute_script(
    constellation_sender: &Sender<ConstellationMsg>,
    webview: &WebViewId,
    js: impl ToString,
) -> WebDriverJSResult {
    let (result_sender, result_receiver) = ipc::channel::<WebDriverJSResult>().unwrap();
    send_to_constellation(
        constellation_sender,
        ConstellationMsg::WebDriverCommand(script_traits::WebDriverCommandMsg::ScriptCommand(
            webview.0,
            WebDriverScriptCommand::ExecuteScript(js.to_string(), result_sender),
        )),
    );
    result_receiver.recv().unwrap()
}
