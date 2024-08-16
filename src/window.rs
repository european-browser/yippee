use std::cell::Cell;

use base::id::WebViewId;
use compositing_traits::ConstellationMsg;
use crossbeam_channel::Sender;
use embedder_traits::{Cursor, EmbedderMsg};
use euclid::{Point2D, Size2D};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use script_traits::{TouchEventType, WheelDelta, WheelMode};
use surfman::Connection;
use surfman::SurfaceType;
use webrender_api::{
    units::{DeviceIntPoint, DeviceIntRect, DeviceIntSize, DevicePoint, LayoutVector2D},
    ScrollLocation,
};
use webrender_traits::RenderingContext;
use winit::{
    dpi::PhysicalPosition,
    event::{ElementState, TouchPhase, WindowEvent},
    event_loop::ActiveEventLoop,
    keyboard::ModifiersState,
    window::{CursorIcon, Window as WinitWindow, WindowId},
};

use crate::{
    compositor::{IOCompositor, MouseWindowEvent},
    keyboard::keyboard_event_from_winit,
    verso::send_to_constellation,
    webview::WebView,
};

use arboard::Clipboard;

/// A Verso window is a Winit window containing several web views.
pub struct Window {
    /// Access to Winit window
    pub(crate) window: WinitWindow,
    /// The main control panel of this window.
    pub(crate) panel: Option<WebView>,
    /// The WebView of this window.
    pub(crate) webview: Option<WebView>,
    /// The mouse physical position in the web view.
    mouse_position: Cell<PhysicalPosition<f64>>,
    /// Modifiers state of the keyboard.
    modifiers_state: Cell<ModifiersState>,
}

impl Window {
    /// Create a Verso window from Winit window and return the rendering context.
    pub fn new(evl: &ActiveEventLoop) -> (Self, RenderingContext) {
        let window = evl
            .create_window(WinitWindow::default_attributes())
            // .with_transparent(true)
            // .with_decorations(false)
            .expect("Failed to create window.");

        let rwh = window.window_handle().expect("Failed to get window handle");
        #[cfg(macos)]
        unsafe {
            if let RawWindowHandle::AppKit(AppKitWindowHandle { ns_view, .. }) = rwh.as_ref() {
                decorate_window(
                    ns_view.as_ptr() as *mut AnyObject,
                    LogicalPosition::new(8.0, 40.0),
                );
            }
        }
        let window_size = window.inner_size();
        let window_size = Size2D::new(window_size.width as i32, window_size.height as i32);
        let display_handle = window
            .display_handle()
            .expect("Failed to get display handle");
        let connection =
            Connection::from_display_handle(display_handle).expect("Failed to create connection");
        let adapter = connection
            .create_adapter()
            .expect("Failed to create adapter");
        let native_widget = connection
            .create_native_widget_from_window_handle(rwh, window_size)
            .expect("Failed to create native widget");
        let surface_type = SurfaceType::Widget { native_widget };
        let rendering_context = RenderingContext::create(&connection, &adapter, surface_type)
            .expect("Failed to create rendering context");
        log::trace!("Created rendering context for window {:?}", window);

        let size = window.inner_size();
        let size = Size2D::new(size.width as i32, size.height as i32);
        (
            Self {
                window,
                panel: Some(WebView::new_panel(DeviceIntRect::from_size(size))),
                webview: None,
                mouse_position: Cell::new(PhysicalPosition::default()),
                modifiers_state: Cell::new(ModifiersState::default()),
            },
            rendering_context,
        )
    }

    /// Create a Verso window with the rendering context.
    pub fn new_with_compositor(evl: &ActiveEventLoop, compositor: &mut IOCompositor) -> Self {
        let window = evl
            .create_window(WinitWindow::default_attributes())
            // .with_transparent(true)
            // .with_decorations(false)
            .expect("Failed to create window.");

        let rwh = window.window_handle().expect("Failed to get window handle");
        #[cfg(macos)]
        unsafe {
            if let RawWindowHandle::AppKit(AppKitWindowHandle { ns_view, .. }) = rwh.as_ref() {
                decorate_window(
                    ns_view.as_ptr() as *mut AnyObject,
                    LogicalPosition::new(8.0, 40.0),
                );
            }
        }
        let window_size = window.inner_size();
        let window_size = Size2D::new(window_size.width as i32, window_size.height as i32);
        let native_widget = compositor
            .rendering_context
            .connection()
            .create_native_widget_from_window_handle(rwh, window_size)
            .expect("Failed to create native widget");
        let surface_type = SurfaceType::Widget { native_widget };
        let surface = compositor
            .rendering_context
            .create_surface(surface_type)
            .ok();
        compositor.surfaces.insert(window.id(), surface);
        Self {
            window,
            panel: None,
            webview: None,
            mouse_position: Cell::new(PhysicalPosition::default()),
            modifiers_state: Cell::new(ModifiersState::default()),
        }
    }

    /// Handle Winit window event and return a boolean to indicate if the compositor should repaint immediately.
    pub fn handle_winit_window_event(
        &mut self,
        sender: &Sender<ConstellationMsg>,
        compositor: &mut IOCompositor,
        event: &winit::event::WindowEvent,
    ) -> bool {
        match event {
            WindowEvent::Focused(focused) => {
                if *focused {
                    compositor.swap_current_window(self);
                }
            }
            WindowEvent::Resized(size) => {
                let size = Size2D::new(size.width, size.height);
                return compositor.resize(size.to_i32(), self);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                compositor.on_scale_factor_event(*scale_factor as f32, self);
            }
            WindowEvent::CursorEntered { .. } => {
                compositor.swap_current_window(self);
            }
            WindowEvent::CursorMoved { position, .. } => {
                let cursor: DevicePoint = DevicePoint::new(position.x as f32, position.y as f32);
                self.mouse_position.set(*position);
                compositor.on_mouse_window_move_event_class(cursor);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button: script_traits::MouseButton = match button {
                    winit::event::MouseButton::Left => script_traits::MouseButton::Left,
                    winit::event::MouseButton::Right => script_traits::MouseButton::Right,
                    winit::event::MouseButton::Middle => script_traits::MouseButton::Middle,
                    _ => {
                        log::trace!(
                            "Verso Window isn't supporting this mouse button yet: {button:?}"
                        );
                        return false;
                    }
                };
                let position = Point2D::new(
                    self.mouse_position.get().x as f32,
                    self.mouse_position.get().y as f32,
                );

                let event: MouseWindowEvent = match state {
                    ElementState::Pressed => MouseWindowEvent::MouseDown(button, position),
                    ElementState::Released => MouseWindowEvent::MouseUp(button, position),
                };
                compositor.on_mouse_window_event_class(event);

                // Winit didn't send click event, so we send it after mouse up
                if *state == ElementState::Released {
                    let event: MouseWindowEvent = MouseWindowEvent::Click(button, position);
                    compositor.on_mouse_window_event_class(event);
                }
            }
            WindowEvent::PinchGesture { delta, .. } => {
                compositor.on_zoom_window_event(1.0 + *delta as f32, self);
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                // FIXME: Pixels per line, should be configurable (from browser setting?) and vary by zoom level.
                const LINE_HEIGHT: f32 = 38.0;

                let (mut x, mut y, mode) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => {
                        (*x as f64, (*y * LINE_HEIGHT) as f64, WheelMode::DeltaLine)
                    }
                    winit::event::MouseScrollDelta::PixelDelta(position) => {
                        let position = position.to_logical::<f64>(self.window.scale_factor());
                        (position.x, position.y, WheelMode::DeltaPixel)
                    }
                };

                // Wheel Event
                compositor.on_wheel_event(
                    WheelDelta { x, y, z: 0.0, mode },
                    DevicePoint::new(
                        self.mouse_position.get().x as f32,
                        self.mouse_position.get().y as f32,
                    ),
                );

                // Scroll Event
                // Do one axis at a time.
                if y.abs() >= x.abs() {
                    x = 0.0;
                } else {
                    y = 0.0;
                }

                let phase: TouchEventType = match phase {
                    TouchPhase::Started => TouchEventType::Down,
                    TouchPhase::Moved => TouchEventType::Move,
                    TouchPhase::Ended => TouchEventType::Up,
                    TouchPhase::Cancelled => TouchEventType::Cancel,
                };

                compositor.on_scroll_event(
                    ScrollLocation::Delta(LayoutVector2D::new(x as f32, y as f32)),
                    DeviceIntPoint::new(
                        self.mouse_position.get().x as i32,
                        self.mouse_position.get().y as i32,
                    ),
                    phase,
                );
            }
            WindowEvent::ModifiersChanged(modifier) => self.modifiers_state.set(modifier.state()),
            WindowEvent::KeyboardInput { event, .. } => {
                let event = keyboard_event_from_winit(event, self.modifiers_state.get());
                log::trace!("Verso is handling {:?}", event);
                let msg = ConstellationMsg::Keyboard(event);
                send_to_constellation(sender, msg);
            }
            e => log::trace!("Verso Window isn't supporting this window event yet: {e:?}"),
        }
        false
    }

    /// Handle servo messages. Return true if it requests a new window
    pub fn handle_servo_message(
        &mut self,
        webview_id: WebViewId,
        message: EmbedderMsg,
        sender: &Sender<ConstellationMsg>,
        clipboard: Option<&mut Clipboard>,
        compositor: &mut IOCompositor,
    ) -> bool {
        // // Handle message in Verso Panel
        if let Some(panel) = &self.panel {
            if panel.webview_id == webview_id {
                return self.handle_servo_messages_with_panel(
                    webview_id, message, sender, clipboard, compositor,
                );
            }
        }
        // Handle message in Verso WebView
        self.handle_servo_messages_with_webview(webview_id, message, sender, clipboard, compositor);
        false
    }

    /// Queues a Winit `WindowEvent::RedrawRequested` event to be emitted that aligns with the windowing system drawing loop.
    pub fn request_redraw(&self) {
        self.window.request_redraw()
    }

    /// Size of the window that's used by webrender.
    pub fn size(&self) -> DeviceIntSize {
        let size = self.window.inner_size();
        Size2D::new(size.width as i32, size.height as i32)
    }

    /// Get Winit window ID of the window.
    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    /// Scale factor of the window. This is also known as HIDPI.
    pub fn scale_factor(&self) -> f64 {
        self.window.scale_factor()
    }

    /// Check if the window has such webview.
    pub fn has_webview(&self, id: WebViewId) -> bool {
        self.panel.as_ref().map_or(false, |w| w.webview_id == id)
            || self.webview.as_ref().map_or(false, |w| w.webview_id == id)
    }

    /// Remove the webview in this window by provided webview ID. If this is the panel, it will
    /// shut down the compositor and then close whole application.
    pub fn remove_webview(
        &mut self,
        id: WebViewId,
        compositor: &mut IOCompositor,
    ) -> (Option<WebView>, bool) {
        if self.panel.as_ref().filter(|w| w.webview_id == id).is_some() {
            if let Some(w) = self.webview.as_ref() {
                send_to_constellation(
                    &compositor.constellation_chan,
                    ConstellationMsg::CloseWebView(w.webview_id),
                )
            }
            (self.panel.take(), false)
        } else if self
            .webview
            .as_ref()
            .filter(|w| w.webview_id == id)
            .is_some()
        {
            (self.webview.take(), self.panel.is_none())
        } else {
            (None, false)
        }
    }

    /// Get the painting order of this window.
    pub fn painting_order(&self) -> Vec<&WebView> {
        let mut order = vec![];
        if let Some(webview) = &self.webview {
            order.push(webview);
        }
        if let Some(panel) = &self.panel {
            order.push(panel);
        }
        order
    }

    /// Set cursor icon of the window.
    pub fn set_cursor_icon(&self, cursor: Cursor) {
        let winit_cursor = match cursor {
            Cursor::Default => CursorIcon::Default,
            Cursor::Pointer => CursorIcon::Pointer,
            Cursor::ContextMenu => CursorIcon::ContextMenu,
            Cursor::Help => CursorIcon::Help,
            Cursor::Progress => CursorIcon::Progress,
            Cursor::Wait => CursorIcon::Wait,
            Cursor::Cell => CursorIcon::Cell,
            Cursor::Crosshair => CursorIcon::Crosshair,
            Cursor::Text => CursorIcon::Text,
            Cursor::VerticalText => CursorIcon::VerticalText,
            Cursor::Alias => CursorIcon::Alias,
            Cursor::Copy => CursorIcon::Copy,
            Cursor::Move => CursorIcon::Move,
            Cursor::NoDrop => CursorIcon::NoDrop,
            Cursor::NotAllowed => CursorIcon::NotAllowed,
            Cursor::Grab => CursorIcon::Grab,
            Cursor::Grabbing => CursorIcon::Grabbing,
            Cursor::EResize => CursorIcon::EResize,
            Cursor::NResize => CursorIcon::NResize,
            Cursor::NeResize => CursorIcon::NeResize,
            Cursor::NwResize => CursorIcon::NwResize,
            Cursor::SResize => CursorIcon::SResize,
            Cursor::SeResize => CursorIcon::SeResize,
            Cursor::SwResize => CursorIcon::SwResize,
            Cursor::WResize => CursorIcon::WResize,
            Cursor::EwResize => CursorIcon::EwResize,
            Cursor::NsResize => CursorIcon::NsResize,
            Cursor::NeswResize => CursorIcon::NeswResize,
            Cursor::NwseResize => CursorIcon::NwseResize,
            Cursor::ColResize => CursorIcon::ColResize,
            Cursor::RowResize => CursorIcon::RowResize,
            Cursor::AllScroll => CursorIcon::AllScroll,
            Cursor::ZoomIn => CursorIcon::ZoomIn,
            Cursor::ZoomOut => CursorIcon::ZoomOut,
            _ => CursorIcon::Default,
        };
        self.window.set_cursor(winit_cursor);
    }
}

/* window decoration */
#[cfg(macos)]
use objc2::runtime::AnyObject;
#[cfg(macos)]
use raw_window_handle::{AppKitWindowHandle, RawWindowHandle};
#[cfg(macos)]
use winit::dpi::LogicalPosition;

/// Window decoration for macOS.
#[cfg(macos)]
pub unsafe fn decorate_window(view: *mut AnyObject, _position: LogicalPosition<f64>) {
    use objc2::rc::Id;
    use objc2_app_kit::{NSView, NSWindowStyleMask, NSWindowTitleVisibility};

    let ns_view: Id<NSView> = unsafe { Id::retain(view.cast()) }.unwrap();
    let window = ns_view
        .window()
        .expect("view was not installed in a window");
    window.setTitlebarAppearsTransparent(true);
    window.setTitleVisibility(NSWindowTitleVisibility::NSWindowTitleHidden);
    window.setStyleMask(
        NSWindowStyleMask::Titled
            | NSWindowStyleMask::FullSizeContentView
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::Miniaturizable,
    );
}
