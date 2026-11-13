//! See-through windows on macOS: the desktop shows, blurred, through the
//! acrylic chrome (`theme::material`). A behind-window `NSVisualEffectView`
//! sits under everything in the content view, and frames are shown in our
//! own `CALayer` as premultiplied BGRA, because softbuffer's macOS layer
//! drops alpha. Linux and `HUGINN_OPAQUE_UI=1` keep softbuffer.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::LazyLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2::{msg_send, sel};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, CGImageComponentInfo, CGImagePixelFormatInfo,
};
use objc2_quartz_core::{CALayer, CATransaction, kCAGravityTopLeft};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// `NSVisualEffectMaterialHUDWindow`: a dark, strong blur.
const MATERIAL_HUD: isize = 13;
const BLENDING_BEHIND_WINDOW: isize = 0;
const STATE_ACTIVE: isize = 1;
/// `NSViewWidthSizable | NSViewHeightSizable`.
const SIZABLE: usize = 2 | 16;
const WINDOW_BELOW: isize = -1;

pub struct GlassSurface {
    layer: Retained<CALayer>,
    color_space: CFRetained<CGColorSpace>,
    width: usize,
    height: usize,
    /// The frame being composed; handed to Core Graphics on present.
    buffer: Vec<u32>,
}

/// `NSVisualEffectView` that never takes the mouse: clicks and the cursor
/// belong to winit's view it sits in.
static BLUR_VIEW: LazyLock<&'static AnyClass> = LazyLock::new(|| {
    extern "C-unwind" fn hit_test(_this: &AnyObject, _cmd: Sel, _point: CGPoint) -> *mut AnyObject {
        ptr::null_mut()
    }
    let superclass = AnyClass::get(c"NSVisualEffectView").expect("AppKit is loaded");
    let mut builder =
        ClassBuilder::new(c"HuginnBlurView", superclass).expect("class registered once");
    // SAFETY: the signature matches `-[NSView hitTest:]`.
    unsafe {
        builder.add_method(
            sel!(hitTest:),
            hit_test as extern "C-unwind" fn(_, _, _) -> _,
        );
    }
    builder.register()
});

impl GlassSurface {
    /// Make `window` see-through (it must be created transparent) and set up
    /// the layer frames go to.
    pub fn new(window: &Window) -> Result<GlassSurface, String> {
        let handle = window
            .window_handle()
            .map_err(|e| format!("window handle: {e}"))?;
        let RawWindowHandle::AppKit(h) = handle.as_raw() else {
            return Err("not an AppKit window".into());
        };
        // SAFETY: winit's content view, alive as long as the window.
        let view: &AnyObject = unsafe { h.ns_view.cast::<AnyObject>().as_ref() };
        // SAFETY: plain AppKit messages with their documented argument types,
        // on the main thread (winit creates windows there).
        let root = unsafe {
            let _: () = msg_send![view, setWantsLayer: Bool::YES];
            let bounds: CGRect = msg_send![view, bounds];
            let blur: Retained<AnyObject> = msg_send![*BLUR_VIEW, new];
            let _: () = msg_send![&*blur, setFrame: bounds];
            let _: () = msg_send![&*blur, setAutoresizingMask: SIZABLE];
            let _: () = msg_send![&*blur, setMaterial: MATERIAL_HUD];
            let _: () = msg_send![&*blur, setBlendingMode: BLENDING_BEHIND_WINDOW];
            let _: () = msg_send![&*blur, setState: STATE_ACTIVE];
            let _: () = msg_send![
                view,
                addSubview: &*blur,
                positioned: WINDOW_BELOW,
                relativeTo: Option::<&AnyObject>::None
            ];
            // Dark appearance: the blur and the traffic lights match the chrome.
            let name: *mut AnyObject = msg_send![
                AnyClass::get(c"NSString").expect("Foundation"),
                stringWithUTF8String: c"NSAppearanceNameDarkAqua".as_ptr()
            ];
            let dark: *mut AnyObject = msg_send![
                AnyClass::get(c"NSAppearance").expect("AppKit"),
                appearanceNamed: name
            ];
            let win: *mut AnyObject = msg_send![view, window];
            if !win.is_null() && !dark.is_null() {
                let _: () = msg_send![win, setAppearance: dark];
            }
            let root: Option<Retained<CALayer>> = msg_send![view, layer];
            root.ok_or("the content view has no layer")?
        };
        let layer = CALayer::new();
        layer.setAnchorPoint(CGPoint::new(0.0, 0.0));
        layer.setGeometryFlipped(true);
        // SAFETY: a constant string from QuartzCore.
        layer.setContentsGravity(unsafe { kCAGravityTopLeft });
        // Above the blur view's layer, whatever order AppKit keeps.
        layer.setZPosition(1.0);
        root.addSublayer(&layer);
        Ok(GlassSurface {
            layer,
            color_space: CGColorSpace::new_device_rgb().ok_or("no RGB color space")?,
            width: 0,
            height: 0,
            buffer: Vec::new(),
        })
    }

    /// The window is `width` x `height` device px at `scale`.
    pub fn resize(&mut self, width: u32, height: u32, scale: f64) {
        self.width = width as usize;
        self.height = height as usize;
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.layer.setContentsScale(scale);
        self.layer.setPosition(CGPoint::new(0.0, 0.0));
        self.layer.setBounds(CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(f64::from(width) / scale, f64::from(height) / scale),
        ));
        CATransaction::commit();
    }

    /// The next frame, `width * height` pixels in the window format
    /// (`ui::Pixels`: transparency in the top byte).
    pub fn buffer(&mut self) -> &mut [u32] {
        self.buffer.resize(self.width * self.height, 0);
        &mut self.buffer
    }

    /// Show the frame from [`buffer`](Self::buffer).
    pub fn present(&mut self) -> Result<(), String> {
        if self.buffer.len() != self.width * self.height || self.buffer.is_empty() {
            return Ok(());
        }
        // Transparency to alpha: premultiplied BGRA for Core Graphics.
        for p in &mut self.buffer {
            *p ^= 0xFF00_0000;
        }
        unsafe extern "C-unwind" fn release(
            _info: *mut c_void,
            data: NonNull<c_void>,
            size: usize,
        ) {
            let slice = ptr::slice_from_raw_parts_mut(data.cast::<u32>().as_ptr(), size / 4);
            // SAFETY: the box handed over below, released once by Core Graphics.
            drop(unsafe { Box::from_raw(slice) });
        }
        let pixels = std::mem::take(&mut self.buffer).into_boxed_slice();
        let len = pixels.len() * 4;
        let data = Box::into_raw(pixels).cast::<c_void>();
        // SAFETY: `data` is valid for `len` bytes until `release` runs.
        let provider =
            unsafe { CGDataProvider::with_data(ptr::null_mut(), data, len, Some(release)) }
                .ok_or("CGDataProvider failed")?;
        let info = CGBitmapInfo(
            CGImageAlphaInfo::PremultipliedFirst.0
                | CGImageComponentInfo::Integer.0
                | CGImageByteOrderInfo::Order32Little.0
                | CGImagePixelFormatInfo::Packed.0,
        );
        // SAFETY: the provider holds width * height 32-bit pixels.
        let image = unsafe {
            CGImage::new(
                self.width,
                self.height,
                8,
                32,
                self.width * 4,
                Some(&self.color_space),
                info,
                Some(&provider),
                ptr::null(),
                false,
                CGColorRenderingIntent::RenderingIntentDefault,
            )
        }
        .ok_or("CGImage failed")?;
        // No implicit fade between frames.
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        // SAFETY: a CGImage is valid layer contents.
        unsafe { self.layer.setContents(Some(image.as_ref())) };
        CATransaction::commit();
        Ok(())
    }
}
