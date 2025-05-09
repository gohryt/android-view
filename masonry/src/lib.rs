// Copyright 2024 the Xilem Authors
// SPDX-License-Identifier: Apache-2.0

use accesskit::{ActionHandler, ActionRequest, ActivationHandler, TreeUpdate};
use android_view::{
    jni::sys::{jint, jlong},
    ndk::{event::Keycode, native_window::NativeWindow},
    *,
};
use masonry::{
    Handled,
    app::{RenderRoot, RenderRootOptions, RenderRootSignal, WindowSizePolicy},
    core::{TextEvent, Widget, WindowEvent},
    dpi::PhysicalSize,
    peniko::Color,
};
use tracing::{debug, info, info_span};
use vello::{
    Renderer, RendererOptions,
    util::{RenderContext, RenderSurface},
    wgpu::{
        self, PresentMode,
        rwh::{DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle},
    },
};

mod app_driver;
pub use app_driver::*;

// From VelloCompose
struct AndroidWindowHandle {
    window: NativeWindow,
}

impl HasDisplayHandle for AndroidWindowHandle {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::android())
    }
}

impl HasWindowHandle for AndroidWindowHandle {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        self.window.window_handle()
    }
}

/// Helper function that creates a vello `Renderer` for a given `RenderContext` and `RenderSurface`
fn create_vello_renderer(render_cx: &RenderContext, surface: &RenderSurface<'_>) -> Renderer {
    Renderer::new(
        &render_cx.devices[surface.dev_id].device,
        RendererOptions {
            use_cpu: false,
            antialiasing_support: vello::AaSupport::area_only(),
            num_init_threads: None,
            // TODO: add pipeline cache.
            pipeline_cache: None,
        },
    )
    .expect("Couldn't create renderer")
}

pub struct MasonryState {
    render_cx: RenderContext,
    render_root: RenderRoot,
    tap_counter: TapCounter,
    renderer: Option<Renderer>,
    render_surface: Option<RenderSurface<'static>>,
    accesskit_adapter: accesskit_android::Adapter,
    background_color: Color,
}

impl MasonryState {
    pub fn new(root_widget: impl Widget, background_color: Color) -> Self {
        let render_cx = RenderContext::new();
        // TODO: We can't know this scale factor until later?
        let scale_factor = 1.0;

        Self {
            render_cx,
            render_root: RenderRoot::new(
                root_widget,
                RenderRootOptions {
                    use_system_fonts: true,
                    size_policy: WindowSizePolicy::User,
                    scale_factor,
                    test_font: None,
                },
            ),
            renderer: None,
            tap_counter: TapCounter::default(),
            render_surface: None,
            accesskit_adapter: Default::default(),
            background_color,
        }
    }
}

#[derive(Default)]
struct MasonryAccessActivationHandler {
    requested_initial_tree: bool,
}

impl ActivationHandler for MasonryAccessActivationHandler {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        self.requested_initial_tree = true;
        None
    }
}

struct MasonryAccessActionHandler<'a> {
    render_root: &'a mut RenderRoot,
}

impl ActionHandler for MasonryAccessActionHandler<'_> {
    fn do_action(&mut self, request: ActionRequest) {
        self.render_root.handle_access_event(request);
    }
}

struct MasonryViewPeer<Driver: AppDriver + Send> {
    state: MasonryState,
    app_driver: Driver,
}

impl<Driver: AppDriver + Send> MasonryViewPeer<Driver> {
    fn handle_signals(&mut self, ctx: &mut CallbackCtx) {
        let mut needs_redraw = false;
        while let Some(signal) = self.state.render_root.pop_signal() {
            match signal {
                RenderRootSignal::Action(action, widget_id) => {
                    let mut driver_ctx = DriverCtx {
                        render_root: &mut self.state.render_root,
                    };
                    debug!("Action {:?} on widget {:?}", action, widget_id);
                    self.app_driver
                        .on_action(&mut driver_ctx, widget_id, action);
                }
                RenderRootSignal::StartIme => {
                    // TODO
                }
                RenderRootSignal::EndIme => {
                    // TODO
                }
                RenderRootSignal::ImeMoved(_position, _size) => {
                    // TODO
                }
                RenderRootSignal::RequestRedraw => {
                    needs_redraw = true;
                }
                RenderRootSignal::RequestAnimFrame => {
                    // Does this need to do something different from RequestRedraw?
                    needs_redraw = true;
                }
                RenderRootSignal::TakeFocus => {
                    // TODO
                }
                RenderRootSignal::SetCursor(_cursor) => {
                    // TODO?
                }
                RenderRootSignal::SetSize(_size) => {
                    // TODO: Does this ever apply, maybe for embedded views?
                }
                RenderRootSignal::SetTitle(_title) => {
                    // TODO: Does this ever apply?
                }
                RenderRootSignal::DragWindow => {
                    // TODO: Does this ever apply?
                }
                RenderRootSignal::DragResizeWindow(_direction) => {
                    // TODO: Does this ever apply?
                }
                RenderRootSignal::ToggleMaximized => {
                    // TODO: Does this ever apply?
                }
                RenderRootSignal::Minimize => {
                    // TODO: Does this ever apply?
                }
                RenderRootSignal::Exit => {
                    // TODO: Should we do something with this?
                }
                RenderRootSignal::ShowWindowMenu(_position) => {
                    // TODO: Does this ever apply?
                }
                RenderRootSignal::WidgetSelectedInInspector(widget_id) => {
                    let Some(widget) = self.state.render_root.get_widget(widget_id) else {
                        return;
                    };
                    let widget_name = widget.short_type_name();
                    let display_name = if let Some(debug_text) = widget.get_debug_text() {
                        format!("{widget_name}<{debug_text}>")
                    } else {
                        widget_name.into()
                    };
                    info!("Widget selected in inspector: {widget_id} - {display_name}");
                }
            }
        }

        // If we're processing a lot of actions, we may have a lot of pending redraws.
        // We batch them up to avoid redundant requests.
        if needs_redraw {
            ctx.view.post_frame_callback(&mut ctx.env);
        }
    }

    fn redraw(&mut self, ctx: &mut CallbackCtx) {
        let _span = info_span!("redraw");
        self.state
            .render_root
            .handle_window_event(WindowEvent::AnimFrame);
        let (scene, tree_update) = self.state.render_root.redraw();

        if let Some(events) = self
            .state
            .accesskit_adapter
            .update_if_active(|| tree_update)
        {
            ctx.push_dynamic_deferred_callback(move |env, view| {
                events.raise(env, &view.0);
            });
        }

        // Get the RenderSurface (surface + config).
        let surface = self.state.render_surface.as_ref().unwrap();

        // Get the window size.
        let width = surface.config.width;
        let height = surface.config.height;

        // Get a handle to the device.
        let device_handle = &self.state.render_cx.devices[surface.dev_id];

        // Render to the surface's texture.
        self.state
            .renderer
            .as_mut()
            .unwrap()
            .render_to_texture(
                &device_handle.device,
                &device_handle.queue,
                &scene,
                &surface.target_view,
                &vello::RenderParams {
                    base_color: self.state.background_color,
                    width,
                    height,
                    antialiasing_method: vello::AaConfig::Area,
                },
            )
            .expect("failed to render to surface");

        // Get the surface's texture.
        let surface_texture = surface
            .surface
            .get_current_texture()
            .expect("failed to get surface texture");

        // Perform the copy.
        let mut encoder =
            device_handle
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("Surface Blit"),
                });
        surface.blitter.copy(
            &device_handle.device,
            &mut encoder,
            &surface.target_view,
            &surface_texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default()),
        );
        device_handle.queue.submit([encoder.finish()]);
        // Queue the texture to be presented on the surface.
        surface_texture.present();

        device_handle.device.poll(wgpu::Maintain::Poll);
    }

    fn on_key_event<'local>(
        &mut self,
        ctx: &mut CallbackCtx<'local>,
        event: &KeyEvent<'local>,
    ) -> bool {
        let handled = self
            .state
            .render_root
            .handle_text_event(TextEvent::Keyboard(event.to_keyboard_event(&mut ctx.env)));
        self.handle_signals(ctx);
        matches!(handled, Handled::Yes)
    }

    fn with_access_activation_handler<'local, T>(
        &mut self,
        ctx: &mut CallbackCtx<'local>,
        f: impl FnOnce(
            &mut CallbackCtx<'local>,
            &mut accesskit_android::Adapter,
            &mut MasonryAccessActivationHandler,
        ) -> T,
    ) -> T {
        let mut handler = MasonryAccessActivationHandler::default();
        let result = f(ctx, &mut self.state.accesskit_adapter, &mut handler);
        if handler.requested_initial_tree {
            self.state
                .render_root
                .handle_window_event(WindowEvent::RebuildAccessTree);
            self.handle_signals(ctx);
        }
        result
    }
}

impl<Driver: AppDriver + Send> ViewPeer for MasonryViewPeer<Driver> {
    fn on_key_down<'local>(
        &mut self,
        ctx: &mut CallbackCtx<'local>,
        _: Keycode,
        event: &KeyEvent<'local>,
    ) -> bool {
        self.on_key_event(ctx, event)
    }

    fn on_key_up<'local>(
        &mut self,
        ctx: &mut CallbackCtx<'local>,
        _: Keycode,
        event: &KeyEvent<'local>,
    ) -> bool {
        self.on_key_event(ctx, event)
    }

    fn on_touch_event<'local>(
        &mut self,
        ctx: &mut CallbackCtx<'local>,
        event: &MotionEvent<'local>,
    ) -> bool {
        let Some(ev) = event.to_pointer_event(&mut ctx.env, &self.state.tap_counter.vc) else {
            return false;
        };
        let ev = self.state.tap_counter.attach_count(ev);
        self.state.render_root.handle_pointer_event(ev);
        self.handle_signals(ctx);
        true
    }

    fn on_generic_motion_event<'local>(
        &mut self,
        ctx: &mut CallbackCtx<'local>,
        event: &MotionEvent<'local>,
    ) -> bool {
        self.on_touch_event(ctx, event)
    }

    fn on_hover_event<'local>(
        &mut self,
        ctx: &mut CallbackCtx<'local>,
        event: &MotionEvent<'local>,
    ) -> bool {
        let action = event.action(&mut ctx.env);
        let x = event.x(&mut ctx.env);
        let y = event.y(&mut ctx.env);
        if let Some(events) = self.with_access_activation_handler(ctx, |_ctx, adapter, handler| {
            adapter.on_hover_event(handler, action, x, y)
        }) {
            ctx.push_dynamic_deferred_callback(move |env, view| {
                events.raise(env, &view.0);
            });
            true
        } else {
            self.on_touch_event(ctx, event)
        }
    }

    fn on_focus_changed<'local>(
        &mut self,
        ctx: &mut CallbackCtx<'local>,
        gain_focus: bool,
        _direction: jint,
        _previously_focused_rect: Option<&Rect<'local>>,
    ) {
        self.state
            .render_root
            .handle_text_event(TextEvent::WindowFocusChange(gain_focus));
        self.handle_signals(ctx);
    }

    fn surface_changed<'local>(
        &mut self,
        ctx: &mut CallbackCtx<'local>,
        holder: &SurfaceHolder<'local>,
        _format: jint,
        width: jint,
        height: jint,
    ) {
        self.state.tap_counter = TapCounter::new(ctx.view.view_configuration(&mut ctx.env));
        let size = PhysicalSize {
            width: width as u32,
            height: height as u32,
        };
        self.state
            .render_root
            .handle_window_event(WindowEvent::Resize(size));

        let window = holder.surface(&mut ctx.env).to_native_window(&mut ctx.env);
        // Drop the old surface, if any, that owned the native window
        // before creating a new one. Otherwise, we crash with
        // ERROR_NATIVE_WINDOW_IN_USE_KHR.
        self.state.render_surface = None;
        let surface = self
            .state
            .render_cx
            .instance
            .create_surface(wgpu::SurfaceTarget::from(AndroidWindowHandle { window }))
            .expect("Error creating surface");
        let dev_id = pollster::block_on(self.state.render_cx.device(Some(&surface)))
            .expect("No compatible device");
        let device_handle = &self.state.render_cx.devices[dev_id];
        let capabilities = surface.get_capabilities(device_handle.adapter());
        let present_mode = if capabilities.present_modes.contains(&PresentMode::Mailbox) {
            PresentMode::Mailbox
        } else {
            PresentMode::AutoVsync
        };

        let surface_future = self.state.render_cx.create_render_surface(
            surface,
            width as _,
            height as _,
            present_mode,
        );
        let surface = pollster::block_on(surface_future).expect("Error creating surface");

        // Create a vello Renderer for the surface (using its device id)
        self.state
            .renderer
            .get_or_insert_with(|| create_vello_renderer(&self.state.render_cx, &surface));
        self.state.render_surface = Some(surface);

        self.redraw(ctx);
    }

    fn surface_destroyed<'local>(
        &mut self,
        ctx: &mut CallbackCtx<'local>,
        _holder: &SurfaceHolder<'local>,
    ) {
        self.state.render_surface = None;
        ctx.view.remove_frame_callback(&mut ctx.env);
    }

    fn do_frame(&mut self, ctx: &mut CallbackCtx, _frame_time_nanos: jlong) {
        self.redraw(ctx);
    }

    fn as_accessibility_node_provider(&mut self) -> Option<&mut dyn AccessibilityNodeProvider> {
        Some(self)
    }

    fn as_input_connection(&mut self) -> Option<&mut dyn InputConnection> {
        // TODO
        None
    }
}

impl<Driver: AppDriver + Send> AccessibilityNodeProvider for MasonryViewPeer<Driver> {
    fn create_accessibility_node_info<'local>(
        &mut self,
        ctx: &mut CallbackCtx<'local>,
        virtual_view_id: jint,
    ) -> AccessibilityNodeInfo<'local> {
        self.with_access_activation_handler(ctx, |ctx, adapter, handler| {
            AccessibilityNodeInfo(adapter.create_accessibility_node_info(
                handler,
                &mut ctx.env,
                &ctx.view.0,
                virtual_view_id,
            ))
        })
    }

    fn find_focus<'local>(
        &mut self,
        ctx: &mut CallbackCtx<'local>,
        focus_type: jint,
    ) -> AccessibilityNodeInfo<'local> {
        self.with_access_activation_handler(ctx, |ctx, adapter, handler| {
            AccessibilityNodeInfo(adapter.find_focus(
                handler,
                &mut ctx.env,
                &ctx.view.0,
                focus_type,
            ))
        })
    }

    fn perform_action<'local>(
        &mut self,
        ctx: &mut CallbackCtx<'local>,
        virtual_view_id: jint,
        action: jint,
        arguments: &Bundle<'local>,
    ) -> bool {
        let Some(action) =
            accesskit_android::PlatformAction::from_java(&mut ctx.env, action, &arguments.0)
        else {
            return false;
        };
        let mut action_handler = MasonryAccessActionHandler {
            render_root: &mut self.state.render_root,
        };
        if let Some(events) = self.state.accesskit_adapter.perform_action(
            &mut action_handler,
            virtual_view_id,
            &action,
        ) {
            ctx.push_dynamic_deferred_callback(move |env, view| {
                events.raise(env, &view.0);
            });
            self.handle_signals(ctx);
            true
        } else {
            false
        }
    }
}

// TODO: InputConnection

pub fn new_view_peer(
    root_widget: impl Widget,
    mut app_driver: impl AppDriver + Send + 'static,
    background_color: Color,
) -> jlong {
    let mut state = MasonryState::new(root_widget, background_color);
    app_driver.on_start(&mut state);
    register_view_peer(MasonryViewPeer { state, app_driver })
}
