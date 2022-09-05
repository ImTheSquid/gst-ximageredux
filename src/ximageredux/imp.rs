use std::{sync::{Mutex, atomic::AtomicBool, Arc, MutexGuard}, time::Duration, ffi::CStr};

use derivative::Derivative;
use gst::{glib::{self, error}, subclass::prelude::{ObjectSubclass, ElementImpl, ObjectImpl, GstObjectImpl, ObjectImplExt}, prelude::ToValue, Memory, error_msg, FlowError};
use gst_app::prelude::{BaseSinkExt, BaseSrcExt};
use gst_base::{BaseSrc, subclass::{prelude::BaseSrcImpl, base_src::CreateSuccess}};
use gst_video::ffi::{gst_video_format_from_masks, gst_video_format_to_string};
use once_cell::sync::Lazy;
use anyhow::{Result, bail};
use xcb::x::{GetGeometry, Drawable, GetImage, self};

use gst::gst_error as error;

pub static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ximageredux",
        gst::DebugColorFlags::empty(),
        Some("X11 Window Capture Engine"),
    )
});

type Xid = u32;

#[derive(Derivative)]
#[derivative(Default)]
struct State {
    connection: Option<xcb::Connection>,
    xid: Option<Xid>,
    #[derivative(Default(value="true"))]
    needs_size_update: bool,
    size: Option<Size>,
    framerate: Duration,
    resize_run: Option<Arc<AtomicBool>>
}

#[derive(Default)]
pub struct XImageRedux {
    state: Mutex<State>
}

struct Size {
    width: u16,
    height: u16
}

impl XImageRedux {
    fn get_frame(&self) -> Result<gst::Buffer> {
        self.update_size_if_needed()?;

        let mut state = self.state.lock().unwrap();
        let (conn, xid) = get_connection(&state)?;

        let cookie = conn.send_request(&GetImage {
            format: x::ImageFormat::ZPixmap, // jpg
            drawable: xcb::x::Drawable::Window(unsafe { xcb::XidNew::new(xid) }),
            x: 0,
            y: 0,
            width: state.size.as_ref().unwrap().width,
            height: state.size.as_ref().unwrap().height,
            plane_mask: u32::MAX,
        });

        let reply = match conn.wait_for_reply(cookie) {
            Ok(reply) => reply,
            Err(e) => bail!("Failed to wait for X reply: {}", e)
        };

        Ok(gst::Buffer::from_slice(reply.data().to_owned()))
    }

    fn update_size_if_needed(&self) -> Result<()> {
        if self.state.lock().unwrap().needs_size_update || self.state.lock().unwrap().size.is_none() {
            let _ = self.state.lock().unwrap().size.insert(self.get_size()?);
            self.state.lock().unwrap().needs_size_update = false;
        }

        Ok(())
    }

    fn get_size(&self) -> Result<Size> {
        let state = self.state.lock().unwrap();
        let (conn, xid) = get_connection(&state)?;
        
        let cookie = conn.send_request(&GetGeometry {
            drawable: Drawable::Window(unsafe { xcb::XidNew::new(xid) }),
        });

        let reply = match conn.wait_for_reply(cookie) {
            Ok(reply) => reply,
            Err(e) => bail!("Failed to wait for X reply: {}", e)
        };

        Ok(Size {
            width: reply.width(),
            height: reply.height()
        })
    }

    unsafe fn get_video_format(&self) -> Result<i32> {
        Ok(gst_video_format_from_masks(depth, bpp, endianness, red_mask, green_mask, blue_mask, alpha_mask))
    }
}

fn get_connection<'a>(state: &'a MutexGuard<State>) -> Result<(&'a xcb::Connection, Xid)> {
    let xid = match state.xid {
        Some(xid) => xid,
        None => bail!("XID is not set!"),
    };

    Ok((state.connection.as_ref().unwrap(), xid))
}

#[glib::object_subclass]
impl ObjectSubclass for XImageRedux {
    const NAME: &'static str = "XImageRedux";
    type Type = super::XImageRedux;
    type ParentType = BaseSrc;
}

impl BaseSrcImpl for XImageRedux {
    fn is_seekable(&self, _element: &Self::Type) -> bool {
        false
    }

    fn start(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        let connection = match xcb::Connection::connect(None) {
            Ok((c, _)) => c,
            Err(e) => return Err(gst::error_msg!(
                gst::ResourceError::Failed,
                [&format!("Failed to connect to X11 server: {}", e.to_string())]
            ))
        };

        let _ = state.connection.insert(connection);

        Ok(())
    }

    fn stop(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        self.state.lock().unwrap().connection.take();

        Ok(())
    }

    fn create(
            &self,
            _element: &Self::Type,
            _offset: u64,
            _buffer: Option<&mut gst::BufferRef>,
            _length: u32,
        ) -> Result<gst_base::subclass::base_src::CreateSuccess, gst::FlowError> {
        // Check if time for next frame
        
        
        if let Err(e) = self.update_size_if_needed() {
            error!(CAT, "Failed to resize: {}", e.to_string());
            return Err(gst::FlowError::Error);
        }

        // {
        //     let state = self.state.lock().unwrap();
        //     buffer.set_size(state.size.as_ref().unwrap().width as usize * state.size.as_ref().unwrap().height as usize * 3);
        // }

        let frame = match self.get_frame() {
            Ok(f) => f,
            Err(e) => {
                error!(CAT, "Failed to get frame: {}", e.to_string());
                return Err(FlowError::Error);
            }
        };

        Ok(CreateSuccess::NewBuffer(frame))
    }

    // fn set_caps(&self, element: &Self::Type, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
    //     println!("CAPS: {:#?}", caps);
    //     Ok(())
    // }

    fn caps(&self, element: &Self::Type, filter: Option<&gst::Caps>) -> Option<gst::Caps> {
        if let Err(e) = self.update_size_if_needed() {
            error!(CAT, "Failed to update size: {}", e.to_string());
            return None;
        }

        let state = self.state.lock().unwrap();
        let size = state.size.as_ref().unwrap();

        let fmt = match unsafe { self.get_video_format() } {
            Ok(fmt) => fmt,
            Err(e) => {
                error!(CAT, "Failed to get video format: {}", e.to_string());
                return None;
            }
        };

        let c_str: &CStr = unsafe { CStr::from_ptr(gst_video_format_to_string(fmt)) };

        Some(gst::Caps::new_simple("video/x-raw", &[
            ("format", &c_str.to_str().unwrap()),
            ("width", &(size.width as u32)),
            ("height", &(size.height as u32))
        ]))
    }
}

impl ElementImpl for XImageRedux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "X11 Window Capture Engine",
                "Source/Video",
                "Captures X11 windows",
                "Jack Hogan",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder_full()
                .structure(gst::Structure::builder("video/x-raw")
                    .field("framerate", gst::FractionRange::new(gst::Fraction::new(0, 1), gst::Fraction::new(i32::MAX, 1)))
                    .field("width", gst::IntRange::new(0, i32::MAX))
                    .field("height", gst::IntRange::new(0, i32::MAX))
                    .build()
                ).build();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl ObjectImpl for XImageRedux {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("xid")
                    .nick("XID")
                    .blurb("XID of window to capture")
                    .build()
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _obj: &Self::Type, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "xid" => match value.get::<String>().unwrap().parse::<Xid>() {
                Ok(xid) => {
                    let mut state = self.state.lock().unwrap();
                    let _ = state.xid.insert(xid);
                    state.needs_size_update = true;
                }
                Err(e) => panic!("Failed to parse XID from String: {}", e),
            }
            _ => unimplemented!()
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "xid" => self.state.lock().unwrap().xid.unwrap_or(0).to_value(),
            _ => unimplemented!()
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);
        obj.set_live(true);
        obj.set_format(gst::Format::Bytes);
    }
}

impl GstObjectImpl for XImageRedux {}