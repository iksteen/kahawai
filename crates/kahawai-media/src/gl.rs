//! GL setup shared by playback, capability probes and benchmarks.
//!
//! macOS workers are system daemons with no AppKit event loop or desktop
//! session. Give GStreamer an offscreen CGL share context instead of letting
//! it create a Cocoa display/window. Linux keeps its existing GL backend.

#[cfg(not(target_os = "macos"))]
pub(crate) fn configure_pipeline(_pipeline: &gstreamer::Pipeline) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn configure_pipeline(pipeline: &gstreamer::Pipeline) -> anyhow::Result<()> {
    macos::configure_pipeline(pipeline)
}

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{Context, Result, anyhow, ensure};
    use glib::translate::ToGlibPtr;
    use gstreamer::{self as gst, glib, prelude::*};
    use gstreamer_gl::{self as gst_gl, prelude::*};
    use std::ffi::{CStr, c_char, c_void};

    type CglContext = *mut c_void;
    type CglPixelFormat = *mut c_void;

    #[link(name = "OpenGL", kind = "framework")]
    unsafe extern "C" {
        fn CGLChoosePixelFormat(
            attributes: *const u32,
            format: *mut CglPixelFormat,
            count: *mut i32,
        ) -> i32;
        fn CGLCreateContext(
            format: CglPixelFormat,
            shared: CglContext,
            context: *mut CglContext,
        ) -> i32;
        fn CGLReleasePixelFormat(format: CglPixelFormat);
        fn CGLReleaseContext(context: CglContext);
        fn CGLGetCurrentContext() -> CglContext;
        fn CGLSetCurrentContext(context: CglContext) -> i32;
        fn CGLErrorString(error: i32) -> *const c_char;
    }

    // Apple SDK CGLTypes.h; AllowOfflineRenderers admits renderers not
    // connected to a display. https://developer.apple.com/documentation/appkit/nsopenglpfaallowofflinerenderers
    const ALLOW_OFFLINE_RENDERERS: u32 = 96;
    const OPENGL_PROFILE: u32 = 99;
    const OPENGL_3_2_CORE: u32 = 0x3200;

    fn check(error: i32, operation: &str) -> Result<()> {
        if error != 0 {
            // SAFETY: CGL returns a static error string for every CGLError.
            let detail = unsafe { CStr::from_ptr(CGLErrorString(error)) }.to_string_lossy();
            return Err(anyhow!("{operation}: {detail} ({error})"));
        }
        Ok(())
    }

    struct NativeContext(CglContext);

    impl NativeContext {
        fn new() -> Result<Self> {
            let attributes = [OPENGL_PROFILE, OPENGL_3_2_CORE, ALLOW_OFFLINE_RENDERERS, 0];
            let mut format = std::ptr::null_mut();
            let mut count = 0;
            // SAFETY: attributes are terminated; CGL writes only the two
            // supplied outputs. The selected format is released after creation.
            unsafe {
                check(
                    CGLChoosePixelFormat(attributes.as_ptr(), &mut format, &mut count),
                    "offscreen CGL pixel format",
                )?;
                ensure!(!format.is_null(), "CGL returned no offscreen pixel format");
                let mut context = std::ptr::null_mut();
                let result = CGLCreateContext(format, std::ptr::null_mut(), &mut context);
                CGLReleasePixelFormat(format);
                check(result, "offscreen CGL context")?;
                ensure!(!context.is_null(), "CGL returned no offscreen context");
                Ok(Self(context))
            }
        }
    }

    impl Drop for NativeContext {
        fn drop(&mut self) {
            // SAFETY: this owns one CGL retain. After initialization it is
            // never made current again; GStreamer creates its own shared
            // contexts on GL threads. The wrapper holds this owner until its
            // final unref, including when a pipeline is finalized elsewhere.
            unsafe { CGLReleaseContext(self.0) };
        }
    }

    struct CurrentContext(CglContext);

    impl CurrentContext {
        fn enter(context: CglContext) -> Result<Self> {
            // SAFETY: used only on the initialization thread, before sharing.
            let previous = unsafe { CGLGetCurrentContext() };
            check(
                unsafe { CGLSetCurrentContext(context) },
                "activate offscreen CGL context",
            )?;
            Ok(Self(previous))
        }
    }

    impl Drop for CurrentContext {
        fn drop(&mut self) {
            // SAFETY: restore this thread's original context on every exit.
            unsafe { CGLSetCurrentContext(self.0) };
        }
    }

    pub(super) fn configure_pipeline(pipeline: &gst::Pipeline) -> Result<()> {
        crate::init()?;
        let native = NativeContext::new()?;
        // CGL context creation requires a Cocoa display. The window is
        // separately offscreen: a base display with no native handle makes
        // GLWindow::new choose the dummy window without touching AppKit.
        // The base display advertises CGL's backend without initializing
        // the Cocoa display subclass (which assumes NSApplication exists).
        let display: gst_gl::GLDisplay = glib::Object::new();
        let display_ptr: *mut gst_gl::ffi::GstGLDisplay = display.to_glib_none().0;
        // SAFETY: public field of a fresh display, before publication.
        unsafe { (*display_ptr).type_ = gst_gl::ffi::GST_GL_DISPLAY_TYPE_COCOA };
        display.filter_gl_api(gst_gl::GLAPI::OPENGL3);
        let offscreen_display: gst_gl::GLDisplay = glib::Object::new();
        // SAFETY: public field of a fresh display, before publication.
        let display_ptr: *mut gst_gl::ffi::GstGLDisplay = offscreen_display.to_glib_none().0;
        unsafe { (*display_ptr).type_ = gst_gl::ffi::GST_GL_DISPLAY_TYPE_NONE };
        // SAFETY: qdata below owns the native context for the wrapper's
        // entire lifetime, as required by gst_gl_context_new_wrapped:
        // https://gstreamer.freedesktop.org/documentation/gl/gstglcontext.html#gst_gl_context_new_wrapped
        let wrapped = unsafe {
            gst_gl::GLContext::new_wrapped(
                &display,
                native.0 as usize,
                gst_gl::GLPlatform::CGL,
                gst_gl::GLAPI::OPENGL3,
            )
        }
        .context("wrap offscreen CGL context")?;
        {
            let _current = CurrentContext::enter(native.0)?;
            wrapped.activate(true)?;
            let result = wrapped.fill_info();
            let deactivate = wrapped.activate(false);
            result.context("inspect offscreen GL context")?;
            deactivate?;
        }
        // SAFETY: a private key written once, never read or stolen. The
        // destroy notifier releases CGL only after the wrapper is destroyed.
        unsafe { wrapped.set_qdata(glib::Quark::from_str("kahawai-native-cgl-context"), native) };

        // A wrapped context has no GL thread. Create the managed context
        // sharing its offline pixel format, with an explicit dummy window,
        // and register it so every GL element reuses the same GL thread.
        let managed = gst_gl::GLContext::new(&display);
        managed.set_window(gst_gl::GLWindow::new(&offscreen_display))?;
        managed
            .create(Some(&wrapped))
            .context("create offscreen GL thread")?;
        gst_gl::GLDisplay::add_context(&display.object_lock(), &managed)?;

        let mut display_context = gst::Context::new("gst.gl.GLDisplay", true);
        display_context.get_mut().unwrap().set_gl_display(&display);
        pipeline.set_context(&display_context);
        let mut app_context = gst::Context::new("gst.gl.app_context", true);
        app_context
            .get_mut()
            .unwrap()
            .structure_mut()
            .set("context", &wrapped);
        // Display's registry is weak; the pipeline must own the managed
        // context until its elements have stopped. No display/context cycle.
        app_context
            .get_mut()
            .unwrap()
            .structure_mut()
            .set("kahawai-managed-context", &managed);
        pipeline.set_context(&app_context);
        tracing::debug!("configured offscreen CGL share context");
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn offscreen_context_restores_the_caller_and_has_no_ownership_cycle() {
            crate::init().unwrap();
            // SAFETY: query this thread's current context without changing it.
            let previous = unsafe { CGLGetCurrentContext() };
            for _ in 0..3 {
                let pipeline = gst::Pipeline::new();
                configure_pipeline(&pipeline).unwrap();
                // SAFETY: query the same initialization thread.
                assert_eq!(unsafe { CGLGetCurrentContext() }, previous);
                let context = pipeline.context("gst.gl.app_context").unwrap();
                let wrapped = context
                    .structure()
                    .get::<gst_gl::GLContext>("context")
                    .unwrap();
                let managed = context
                    .structure()
                    .get::<gst_gl::GLContext>("kahawai-managed-context")
                    .unwrap();
                let wrapped_weak = wrapped.downgrade();
                let managed_weak = managed.downgrade();
                drop(wrapped);
                drop(managed);
                drop(context);
                drop(pipeline);
                assert!(wrapped_weak.upgrade().is_none());
                assert!(managed_weak.upgrade().is_none());
            }
        }
    }
}
