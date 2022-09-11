use std::{sync::{Mutex, atomic::{AtomicBool, Ordering}, Arc, MutexGuard}, time::Duration, ffi::CStr, thread::{JoinHandle, self}};

use derivative::Derivative;
use gst::{glib::{self, ffi::{G_LITTLE_ENDIAN, G_BIG_ENDIAN}}, subclass::prelude::{ObjectSubclass, ElementImpl, ObjectImpl, GstObjectImpl, ObjectImplExt}, prelude::{ToValue, ElementExtManual}, FlowError, error_msg};
use gst_app::prelude::BaseSrcExt;
use gst_base::{subclass::{prelude::{BaseSrcImpl, BaseSrcImplExt, PushSrcImpl}, base_src::CreateSuccess}, PushSrc};
use gst_video::ffi::{gst_video_format_from_masks, gst_video_format_to_string};
use once_cell::sync::Lazy;
use anyhow::{Result, bail};
use xcb::{x::{GetGeometry, Drawable, GetImage, self, ImageOrder, ChangeWindowAttributes, Cw, EventMask, QueryPointer}, CookieWithReplyChecked, Connection};
use xcb::x::Event::ConfigureNotify;

use gst::{
    gst_error as error,
    gst_trace as trace
};

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
    screen_num: Option<i32>,
    xid: Option<Xid>,
    #[derivative(Default(value="true"))]
    show_cursor: bool,
    #[derivative(Default(value="true"))]
    needs_size_update: bool,
    start_x: i16,
    start_y: i16,
    size: Option<Size>,
    frame_duration: Duration,
    resize_run: Option<Arc<AtomicBool>>,
    resize_handle: Option<JoinHandle<()>>,
    last_frame: Option<gst::Buffer>
}

#[derive(Default)]
pub struct XImageRedux {
    state: Arc<Mutex<State>>
}

#[derive(Debug, PartialEq, Eq)]
struct Size {
    width: u16,
    height: u16
}

#[derive(Debug, PartialEq, Eq)]
struct Position {
    x: i16,
    y: i16
}

impl XImageRedux {
    fn get_frame(&self) -> Result<gst::Buffer> {
        self.update_size_if_needed()?;

        let state = self.state.lock().unwrap();
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

        let reply = wait_for_reply(conn, cookie)?;

        let mut buf = gst::Buffer::from_slice(reply.data().to_owned());
        let buf = buf.make_mut();
        buf.set_duration(gst::ClockTime::from_mseconds(state.frame_duration.as_millis() as u64));

        Ok(buf.to_owned())
    }

    // Function looks weird to get around mutex issues
    // Returns whether size was updated
    fn update_size_if_needed(&self) -> Result<bool> {
        let should_update = {
            let mut state = self.state.lock().unwrap();

            if state.needs_size_update || state.size.is_none() {
                state.needs_size_update = false;
                true
            } else {
                false
            }
        };

        if should_update {
            let new = self.get_size()?;
            let _ = self.state.lock().unwrap().size.insert(new);
        }

        Ok(should_update)
    }

    fn get_size(&self) -> Result<Size> {
        let mut state = self.state.lock().unwrap();
        let (conn, xid) = get_connection(&state)?;
        
        let cookie = conn.send_request(&GetGeometry {
            drawable: Drawable::Window(unsafe { xcb::XidNew::new(xid) })
        });

        let reply = wait_for_reply(conn, cookie)?;

        state.start_x = reply.x();
        state.start_y = reply.y();

        Ok(Size {
            width: reply.width(),
            height: reply.height()
        })
    }

    fn open_connection(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();

        let (connection, screen_num) = match xcb::Connection::connect(None) {
            Ok((c, s)) => (c, s),
            Err(e) => bail!("Failed to connect to X11 server: {}", e.to_string())
        };

        let _ = state.connection.insert(connection);
        let _ = state.screen_num.insert(screen_num);

        Ok(())
    }

    unsafe fn get_video_format(&self) -> Result<i32> {
        let state = self.state.lock().unwrap();
        let (conn, xid) = get_connection(&state)?;

        let setup = conn.get_setup();
        let mut endianness = match setup.bitmap_format_bit_order() {
            ImageOrder::MsbFirst => G_BIG_ENDIAN,
            ImageOrder::LsbFirst => G_LITTLE_ENDIAN
        };

        let cookie = conn.send_request(&GetGeometry {
            drawable: Drawable::Window(xcb::XidNew::new(xid))
        });

        let geometry_reply = wait_for_reply(conn, cookie)?;

        let bpp = setup.pixmap_formats().iter().find(|fmt| fmt.depth() == geometry_reply.depth()).unwrap().bits_per_pixel();

        let screen = setup.roots().nth(state.screen_num.unwrap() as usize).unwrap();

        let visual = screen.allowed_depths()
            .flat_map(|depth| depth.visuals().into_iter())
            .find(|vis| vis.visual_id() == screen.root_visual())
            .unwrap();

        // Our caps system handles 24/32bpp RGB as big-endian
        let (red_mask, green_mask, blue_mask) = if (bpp == 24 || bpp == 32) && endianness == G_LITTLE_ENDIAN {
            endianness = G_BIG_ENDIAN;
            let mut set = (visual.red_mask().to_be(), visual.green_mask().to_be(), visual.blue_mask().to_be());

            if bpp == 24 {
                set.0 >>= 8;
                set.1 >>= 8;
                set.2 >>= 8;
            }

            set
        } else {
            (visual.red_mask(), visual.green_mask(), visual.blue_mask())
        };

        let alpha_mask = if bpp == 32 {
            !(red_mask | green_mask | blue_mask)
        } else {
            0
        };

        Ok(gst_video_format_from_masks(geometry_reply.depth().into(), bpp.into(), endianness, red_mask, green_mask, blue_mask, alpha_mask))
    }

    // Returns the relative position of the cursor in the window if it's in the window region
    fn cursor_is_in_bounds(&self) -> Result<Option<Position>> {
        let state = self.state.lock().unwrap();
        let (conn, xid) = get_connection(&state)?;
        let win = unsafe { xcb::XidNew::new(xid) };

        let cookie = conn.send_request(&QueryPointer {
            window: win
        });

        let reply = wait_for_reply(conn, cookie)?;

        Ok(if reply.same_screen() && reply.child() == win {
            Some(Position {
                x: reply.win_x(),
                y: reply.win_y(),
            })
        } else { None })
    }
}

fn wait_for_reply<C>(conn: &Connection, cookie: C) -> Result<C::Reply> 
    where C: CookieWithReplyChecked 
    {
        match conn.wait_for_reply(cookie) {
            Ok(reply) => Ok(reply),
            Err(e) => bail!("Failed to wait for X reply: {}", e)
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
    type ParentType = PushSrc;
}

impl PushSrcImpl for XImageRedux {
    fn create(
            &self,
            element: &Self::Type,
            _buffer: Option<&mut gst::BufferRef>,
        ) -> Result<CreateSuccess, gst::FlowError> {
        // Check if time for next frame
        
        match self.update_size_if_needed() {
            Ok(did_update_size) => if did_update_size {
                if let Err(e) = self.negotiate(element) {
                    error!(CAT, "Failed to renegotiate after resize: {}", e.to_string());
                    return Err(gst::FlowError::Error);
                }
            }
            Err(e) => {
                error!(CAT, "Failed to resize: {}", e.to_string());
                return Err(gst::FlowError::Error);
            }
        }

        let frame = match self.get_frame() {
            Ok(f) => f,
            Err(e) => {
                // If failed to get frame, try to use the last one as a temporary measure
                if let Some(buf) = &self.state.lock().unwrap().last_frame {
                    trace!(CAT, "Failed to get frame, but last frame is usable.");
                    return Ok(CreateSuccess::NewBuffer(buf.clone()));
                } else {
                    error!(CAT, "Failed to get frame: {}", e.to_string());
                    return Err(FlowError::Error);
                }
            }
        };

        // Copy cursor in if needed
        if self.state.lock().unwrap().show_cursor {
            match self.cursor_is_in_bounds() {
                Ok(res) => if let Some(pos) = res {
                    
                }
                Err(e) => {
                    error!(CAT, "Failed to get cursor position: {}", e.to_string());
                    return Err(gst::FlowError::Error);
                }
            }
        }

        let _ = self.state.lock().unwrap().last_frame.insert(frame.clone());

        Ok(CreateSuccess::NewBuffer(frame))
    }
}

impl BaseSrcImpl for XImageRedux {
    fn caps(&self, element: &Self::Type, _filter: Option<&gst::Caps>) -> Option<gst::Caps> {
        if self.state.lock().unwrap().connection.is_none() {
            if let Err(e) = self.open_connection() {
                error!(CAT, "Failed to open connection: {}", e);
                return Some(element.pad_template_list()[0].caps())
            }
        }

        if let Err(e) = self.update_size_if_needed() {
            error!(CAT, "Failed to update size: {}", e.to_string());
            return None;
        }

        let fmt = match unsafe { self.get_video_format() } {
            Ok(fmt) => fmt,
            Err(e) => {
                error!(CAT, "Failed to get video format: {}", e.to_string());
                return None;
            }
        };

        let c_str: &CStr = unsafe { CStr::from_ptr(gst_video_format_to_string(fmt)) };

        let state = self.state.lock().unwrap();
        let size = state.size.as_ref().unwrap();

        Some(gst::Caps::new_simple("video/x-raw", &[
            ("format", &c_str.to_str().unwrap()),
            ("width", &(size.width as i32)),
            ("height", &(size.height as i32)),
            ("framerate", &(gst::FractionRange::new(gst::Fraction::new(0, 1), gst::Fraction::new(i32::MAX, 1))))
        ]))
    }

    fn set_caps(&self, _element: &Self::Type, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        if self.state.lock().unwrap().connection.is_none() {
            return Err(gst::LoggableError::new(*CAT, glib::BoolError::new("Not ready!", "imp.rs", "set_caps", 0)));
        }

        let framerate: gst::Fraction = match caps.structure(0).unwrap().value("framerate").unwrap().get() {
            Ok(f) => f,
            Err(e) => return Err(gst::LoggableError::new(*CAT, glib::BoolError::new(format!("Error: {}", e.to_string()), "imp.rs", "set_caps", 0)))
        };

        self.state.lock().unwrap().frame_duration = Duration::from_millis(1000 * framerate.denom() as u64 / framerate.numer() as u64);

        Ok(())
    }

    fn fixate(&self, element: &Self::Type, mut caps: gst::Caps) -> gst::Caps {
        let caps = caps.get_mut().unwrap();

        for i in 0..caps.size() {
            caps.structure_mut(i).unwrap().fixate_field_nearest_fraction("framerate", gst::Fraction::new(25, 1));
        }

        self.parent_fixate(element, caps.to_owned())
    }

    fn start(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        if let Err(e) = self.open_connection() {
            return Err(error_msg!(
                gst::ResourceError::Failed,
                [&e.to_string()]
            ))
        }

        let xid = {
            let state_wrap = self.state.lock().unwrap();
            get_connection(&state_wrap).unwrap().1
        };

        let run = Arc::new(AtomicBool::new(true));
        let _  = self.state.lock().unwrap().resize_run.insert(run.clone());

        let state_arc = self.state.clone();

        let _ = self.state.lock().unwrap().resize_handle.insert(thread::spawn(move || {
            let conn = xcb::Connection::connect(None).unwrap().0;

            conn.send_request(&ChangeWindowAttributes {
                window: unsafe { xcb::XidNew::new(xid) },
                value_list: &[Cw::EventMask(EventMask::STRUCTURE_NOTIFY)]
            });

            // VERY IMPORTANT
            conn.flush().unwrap();

            let mut last_size = None;

            while run.load(Ordering::SeqCst) {
                match conn.poll_for_event() {
                    Ok(e) => if let Some(ev) = e {
                        if let xcb::Event::X(e) = ev {
                            match e {
                                // Listen for size changes
                                ConfigureNotify(e) => {
                                    let size = Size { width: e.width().into(), height: e.height().into() };

                                    // Don't send window relocation events (size stays the same)
                                    if let Some(last_size) = last_size.as_ref() {
                                        if *last_size == size {
                                            continue;
                                        }
                                    } else {
                                        let _ = last_size.insert(size);
                                    }

                                    state_arc.lock().unwrap().needs_size_update = true;
                                }
                                _ => {}
                            }
                        }
                    },
                    Err(e) => {
                        error!(CAT, "Failed to poll for X event: {e}");
                    }
                }

                thread::sleep(Duration::from_millis(50));
            }
        }));

        Ok(())
    }

    fn stop(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        if let Some(run) = self.state.lock().unwrap().resize_run.take() {
            run.store(false, Ordering::SeqCst);
        }

        if let Some(handle) = self.state.lock().unwrap().resize_handle.take() {
            handle.join().unwrap();
        }

        self.state.lock().unwrap().connection.take();

        Ok(())
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
            "xid" => {
                let string = value.get::<String>().unwrap();
                match string.parse::<Xid>() {
                    Ok(xid) => {
                        let mut state = self.state.lock().unwrap();
                        let _ = state.xid.insert(xid);
                        state.needs_size_update = true;
                    }
                    Err(_) => {
                        let no_prefix = string.trim_start_matches("0x");
                        match Xid::from_str_radix(no_prefix, 16) {
                            Ok(xid) => {
                                let mut state = self.state.lock().unwrap();
                                let _ = state.xid.insert(xid);
                                state.needs_size_update = true;
                            }
                            Err(_) => panic!("Failed to parse XID from String"),
                        }
                    },
                }
            }
            "show-cursor" => {
                match value.get::<bool>() {
                    Ok(show) => self.state.lock().unwrap().show_cursor = show,
                    Err(_) => panic!("Failed to parse show-cursor from property"),
                }
            }
            _ => unimplemented!()
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "xid" => self.state.lock().unwrap().xid.unwrap_or(0).to_value(),
            "show-cursor" => self.state.lock().unwrap().show_cursor.to_value(),
            _ => unimplemented!()
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);
        obj.set_live(true);
        obj.set_format(gst::Format::Time);
    }
}

impl GstObjectImpl for XImageRedux {}