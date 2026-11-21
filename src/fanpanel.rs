// Fan control popup: appears when hovering the FAN meter.
//
//  * mouse: click / drag the slider, mouse wheel while hovering, "Auto" button
//  * keyboard (after clicking the popup so it has focus): arrows +-1, PgUp/PgDn +-10,
//    Home = minimum, End = 100 %, A = Auto, Esc = close
//
// The speed is applied through LibreHardwareMonitor (see sensors.rs). Safety: never below MIN_PCT,
// falls back to Auto if the CPU gets hot or LibreHardwareMonitor goes away, and Auto is restored on exit.
use core::mem::{size_of, zeroed};
use core::ptr::{null, null_mut};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, Sender};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::{blend, rr_sdf, wide};
use crate::sensors;

pub const MIN_PCT: i32 = 20;
pub const MAX_PCT: i32 = 100;
const HOT_C: f32 = 85.0;
const ACCENT: (u32, u32, u32) = (236, 214, 84);

struct Panel {
    hwnd: HWND,
    scale: f32,
    rpm: i32,       // -1 unknown
    temp: f32,      // < 0 unknown
    lhm_ok: bool,   // LibreHardwareMonitor reachable
    manual: i32,    // -1 = automatic, otherwise our commanded percent
    dragging: bool,
    anchor: RECT,   // screen rect of the FAN meter
    out_ticks: u32,
    note: &'static str,
    tx: Option<Sender<i32>>,
    font_label: HFONT,
    font_value: HFONT,
    font_small: HFONT,
    font_px: i32,
}

static mut P: Panel = Panel {
    hwnd: null_mut(),
    scale: 1.0,
    rpm: -1,
    temp: -1.0,
    lhm_ok: false,
    manual: -1,
    dragging: false,
    anchor: RECT { left: 0, top: 0, right: 0, bottom: 0 },
    out_ticks: 0,
    note: "",
    tx: None,
    font_label: null_mut(),
    font_value: null_mut(),
    font_small: null_mut(),
    font_px: 0,
};

fn p() -> &'static mut Panel {
    unsafe { &mut *core::ptr::addr_of_mut!(P) }
}

const CLASS: &str = "ActivityMonitorFanPanel";
const ID_HOVER: usize = 7;

// design units (96 dpi)
const W: f32 = 268.0;
const H: f32 = 138.0;
const PADX: f32 = 16.0;

fn control_thread(rx: Receiver<i32>) {
    while let Ok(mut v) = rx.recv() {
        while let Ok(n) = rx.try_recv() {
            v = n; // only the latest request matters
        }
        sensors::set_control(v);
        // LibreHardwareMonitor falls behind (and keeps applying stale values for seconds) when it is
        // flooded with requests, e.g. from a fast mouse wheel, so send at most ~4 per second.
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

pub fn init() {
    unsafe {
        let hinst = GetModuleHandleW(null());
        let cls = wide(CLASS);
        let wc = WNDCLASSW {
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(proc_),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: cls.as_ptr(),
        };
        RegisterClassW(&wc);
        let (tx, rx) = channel();
        p().tx = Some(tx);
        std::thread::spawn(move || control_thread(rx));
    }
}

fn send(v: i32) {
    if let Some(tx) = &p().tx {
        let _ = tx.send(v);
    }
}

fn set_manual(pct: i32) {
    let pct = pct.clamp(MIN_PCT, MAX_PCT);
    let q = p();
    q.note = "";
    if q.manual != pct {
        q.manual = pct;
        send(pct);
    }
    redraw();
}

fn set_auto(note: &'static str) {
    let q = p();
    q.note = note;
    if q.manual >= 0 {
        q.manual = -1;
        send(-1);
    }
    redraw();
}

fn redraw() {
    unsafe {
        if !p().hwnd.is_null() && IsWindowVisible(p().hwnd) != 0 {
            InvalidateRect(p().hwnd, null(), 0);
        }
    }
}

fn current_pct() -> i32 {
    let q = p();
    if q.manual >= 0 {
        return q.manual;
    }
    let t = sensors::CONTROL_TENTHS.load(Ordering::Relaxed);
    if t == sensors::NO_TEMP {
        MIN_PCT
    } else {
        ((t as f32 / 10.0).round() as i32).clamp(MIN_PCT, MAX_PCT)
    }
}

fn control_ok() -> bool {
    p().lhm_ok && sensors::control_available()
}

/// Called every tick with fresh readings. Applies the safety rules.
pub fn update(rpm: i32, temp: f32, lhm_ok: bool) {
    let q = p();
    q.rpm = rpm;
    q.temp = temp;
    q.lhm_ok = lhm_ok;
    if q.manual >= 0 {
        if !lhm_ok || !sensors::control_available() {
            q.manual = -1; // LibreHardwareMonitor restarted/quit: it dropped our setting
            q.note = "Back to Auto: sensor feed lost";
        } else if temp >= HOT_C {
            set_auto("Back to Auto: CPU is hot");
        }
    }
    redraw();
}

/// Mouse wheel while hovering the FAN meter / popup.
pub fn wheel(steps: i32) {
    if control_ok() {
        set_manual(current_pct() + steps);
    }
}

pub fn shutdown() {
    if p().manual >= 0 {
        p().manual = -1;
        sensors::set_control(-1);
    }
}

fn make_font(px: f32, scale: f32, weight: i32) -> HFONT {
    unsafe {
        let face = wide("Segoe UI");
        CreateFontW(
            -((px * scale).round() as i32),
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            face.as_ptr(),
        )
    }
}

pub fn is_visible() -> bool {
    unsafe { !p().hwnd.is_null() && IsWindowVisible(p().hwnd) != 0 }
}

pub fn show(anchor: RECT, scale: f32, take_focus: bool) {
    unsafe {
        let q = p();
        q.anchor = anchor;
        let w = (W * scale).round() as i32;
        let h = (H * scale).round() as i32;
        if q.hwnd.is_null() {
            let cls = wide(CLASS);
            q.hwnd = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST, cls.as_ptr(), null(), WS_POPUP, 0, 0, w, h, null_mut(), null_mut(), GetModuleHandleW(null()), null());
        }
        if q.font_px != (scale * 100.0) as i32 {
            for f in [q.font_label, q.font_value, q.font_small] {
                if !f.is_null() {
                    DeleteObject(f);
                }
            }
            q.font_label = make_font(12.0, scale, 400);
            q.font_value = make_font(18.0, scale, 400);
            q.font_small = make_font(11.0, scale, 400);
            q.font_px = (scale * 100.0) as i32;
        }
        q.scale = scale;
        let cx = (anchor.left + anchor.right) / 2;
        let gap = (6.0 * scale) as i32;
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let x = (cx - w / 2).clamp(4, (screen_w - w - 4).max(4));
        let y = anchor.top - h - gap;
        let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, (10.0 * scale) as i32, (10.0 * scale) as i32);
        SetWindowRgn(q.hwnd, rgn, 1);
        let was = IsWindowVisible(q.hwnd) != 0;
        SetWindowPos(q.hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        if !was {
            q.out_ticks = 0;
            SetTimer(q.hwnd, ID_HOVER, 150, None);
        }
        if take_focus {
            SetForegroundWindow(q.hwnd);
            SetFocus(q.hwnd);
        }
        InvalidateRect(q.hwnd, null(), 0);
    }
}

pub fn hide() {
    unsafe {
        let q = p();
        if !q.hwnd.is_null() {
            KillTimer(q.hwnd, ID_HOVER);
            ShowWindow(q.hwnd, SW_HIDE);
            q.dragging = false;
        }
    }
}

// ------------------------------------------------------------------ geometry (device pixels)
struct Geo {
    x0: f32,
    x1: f32,
    yc: f32,
    btn: (f32, f32, f32, f32),
}

fn geo(s: f32) -> Geo {
    Geo { x0: PADX * s, x1: (W - PADX) * s, yc: 84.0 * s, btn: ((W - PADX - 58.0) * s, 36.0 * s, (W - PADX) * s, 60.0 * s) }
}

fn pct_to_x(g: &Geo, pct: i32) -> f32 {
    g.x0 + (pct - MIN_PCT) as f32 / (MAX_PCT - MIN_PCT) as f32 * (g.x1 - g.x0)
}

fn x_to_pct(g: &Geo, x: f32) -> i32 {
    let t = ((x - g.x0) / (g.x1 - g.x0)).clamp(0.0, 1.0);
    MIN_PCT + (t * (MAX_PCT - MIN_PCT) as f32).round() as i32
}

// ------------------------------------------------------------------ painting
fn paint(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        let q = p();
        let s = q.scale;
        let (w, h) = ((W * s).round() as i32, (H * s).round() as i32);

        let mut bi: BITMAPINFO = zeroed();
        bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut core::ffi::c_void = null_mut();
        let mdc = CreateCompatibleDC(hdc);
        let bmp = CreateDIBSection(mdc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        let old = SelectObject(mdc, bmp);
        let px = core::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
        let (wf, hf) = (w as f32, h as f32);
        let g = geo(s);
        let ok = control_ok();
        let auto = q.manual < 0;
        let pct = current_pct();
        let (ar, ag, ab) = ACCENT;

        // background + border
        for y in 0..h {
            let t = y as f32 / hf;
            let (r, gg, b) = (34.0 - 10.0 * t, 34.0 - 10.0 * t, 40.0 - 12.0 * t);
            for x in 0..w {
                let sdf = rr_sdf(x as f32 + 0.5, y as f32 + 0.5, wf, hf, 9.0 * s);
                let cov = (0.5 - sdf).clamp(0.0, 1.0);
                let bcov = cov - (0.5 - (sdf + s.max(1.0))).clamp(0.0, 1.0);
                let mut c: u32 = 0xFF00_0000 | ((r as u32) << 16) | ((gg as u32) << 8) | b as u32;
                if bcov > 0.0 {
                    blend(&mut c, ar, ag, ab, (200.0 * bcov) as u32);
                }
                px[(y * w + x) as usize] = c;
            }
        }

        if ok {
            // Auto button
            let (bx0, by0, bx1, by1) = g.btn;
            for y in (by0 as i32 - 1)..(by1 as i32 + 1) {
                for x in (bx0 as i32 - 1)..(bx1 as i32 + 1) {
                    let sdf = rr_sdf(x as f32 + 0.5 - bx0, y as f32 + 0.5 - by0, bx1 - bx0, by1 - by0, (by1 - by0) * 0.5);
                    let cov = (0.5 - sdf).clamp(0.0, 1.0);
                    if cov > 0.0 {
                        let i = (y * w + x) as usize;
                        if auto {
                            blend(&mut px[i], ar, ag, ab, (255.0 * cov) as u32);
                        } else {
                            blend(&mut px[i], 70, 70, 80, (255.0 * cov) as u32);
                        }
                    }
                }
            }
            // slider track + fill
            let th = 4.0 * s;
            let tx = pct_to_x(&g, pct);
            for y in (g.yc - 9.0 * s) as i32..(g.yc + 9.0 * s) as i32 {
                for x in (g.x0 - 9.0 * s) as i32..(g.x1 + 9.0 * s) as i32 {
                    let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                    let i = (y * w + x) as usize;
                    let sdf = rr_sdf(fx - g.x0, fy - (g.yc - th * 0.5), g.x1 - g.x0, th, th * 0.5);
                    let cov = (0.5 - sdf).clamp(0.0, 1.0);
                    if cov > 0.0 {
                        blend(&mut px[i], 78, 78, 90, (255.0 * cov) as u32);
                        if fx < tx {
                            let a = if auto { 110.0 } else { 255.0 };
                            blend(&mut px[i], ar, ag, ab, (a * cov) as u32);
                        }
                    }
                    // thumb
                    let d = ((fx - tx) * (fx - tx) + (fy - g.yc) * (fy - g.yc)).sqrt();
                    let r_out = 8.0 * s;
                    let cov_out = (r_out - d + 0.5).clamp(0.0, 1.0);
                    if cov_out > 0.0 {
                        let (tr, tg, tb) = if auto { (150, 150, 160) } else { (ar, ag, ab) };
                        blend(&mut px[i], tr, tg, tb, (255.0 * cov_out) as u32);
                        let cov_in = (r_out - 3.0 * s - d + 0.5).clamp(0.0, 1.0);
                        blend(&mut px[i], 245, 245, 250, (255.0 * cov_in) as u32);
                    }
                }
            }
        }

        // text (GDI ClearType straight onto the opaque pixels)
        SetBkMode(mdc, TRANSPARENT as i32);
        let rgb = |r: u32, g: u32, b: u32| r | (g << 8) | (b << 16);
        let light = rgb((ar + 255) / 2 + 10, (ag + 255) / 2 + 10, (ab + 255) / 2 + 10);
        let white = rgb(238, 238, 244);
        let dim = rgb(150, 150, 164);
        let put = |font: HFONT, txt: &str, l: f32, t: f32, r: f32, b: f32, flags: u32, col: u32| {
            SelectObject(mdc, font);
            SetTextColor(mdc, col);
            let mut rc = RECT { left: (l * s) as i32, top: (t * s) as i32, right: (r * s) as i32, bottom: (b * s) as i32 };
            DrawTextA(mdc, txt.as_ptr(), txt.len() as i32, &mut rc, flags | DT_SINGLELINE | DT_NOPREFIX);
        };
        put(q.font_label, "CPU fan", PADX, 10.0, 150.0, 30.0, DT_LEFT | DT_VCENTER, light);
        let rpm = if q.rpm >= 0 { format!("{} RPM", q.rpm) } else { "-- RPM".to_string() };
        put(q.font_label, &rpm, 120.0, 10.0, W - PADX, 30.0, DT_RIGHT | DT_VCENTER, white);

        if ok {
            let big = format!("{}%", pct);
            put(q.font_value, &big, PADX, 34.0, 80.0, 62.0, DT_LEFT | DT_VCENTER, white);
            let mode = if auto { "automatic" } else { "manual" };
            put(q.font_small, mode, 66.0, 34.0, 150.0, 62.0, DT_LEFT | DT_BOTTOM, dim);
            let (bx0, by0, bx1, by1) = g.btn;
            let col = if auto { rgb(30, 30, 34) } else { white };
            SelectObject(mdc, q.font_label);
            SetTextColor(mdc, col);
            let mut rc = RECT { left: bx0 as i32, top: by0 as i32, right: bx1 as i32, bottom: by1 as i32 };
            DrawTextA(mdc, b"Auto".as_ptr(), 4, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            put(q.font_small, &format!("{}%", MIN_PCT), PADX - 2.0, 96.0, 80.0, 110.0, DT_LEFT | DT_VCENTER, dim);
            put(q.font_small, &format!("{}%", MAX_PCT), W - PADX - 60.0 + 2.0, 96.0, W - PADX + 2.0, 110.0, DT_RIGHT | DT_VCENTER, dim);
            let hint = if q.note.is_empty() { "Wheel, or click and use arrow keys" } else { q.note };
            let col = if q.note.is_empty() { dim } else { rgb(255, 150, 120) };
            put(q.font_small, hint, PADX, 114.0, W - PADX, 132.0, DT_LEFT | DT_VCENTER, col);
        } else {
            let msg = if !q.lhm_ok { "Fan control needs LibreHardwareMonitor\nwith its Remote Web Server enabled" } else { "No controllable CPU fan found" };
            for (i, line) in msg.split('\n').enumerate() {
                put(q.font_small, line, PADX, 44.0 + 18.0 * i as f32, W - PADX, 62.0 + 18.0 * i as f32, DT_LEFT | DT_VCENTER, dim);
            }
        }

        BitBlt(hdc, 0, 0, w, h, mdc, 0, 0, SRCCOPY);
        SelectObject(mdc, old);
        DeleteObject(bmp);
        DeleteDC(mdc);
        EndPaint(hwnd, &ps);
    }
}

fn point(lp: LPARAM) -> (f32, f32) {
    ((lp & 0xFFFF) as i16 as f32, ((lp >> 16) & 0xFFFF) as i16 as f32)
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let q = p();
    let s = q.scale;
    match msg {
        WM_PAINT => {
            paint(hwnd);
            0
        }
        WM_ERASEBKGND => 1,
        WM_LBUTTONDOWN => {
            SetFocus(hwnd);
            if !control_ok() {
                return 0;
            }
            let g = geo(s);
            let (x, y) = point(lp);
            let (bx0, by0, bx1, by1) = g.btn;
            if x >= bx0 && x <= bx1 && y >= by0 && y <= by1 {
                set_auto("");
            } else if y >= g.yc - 14.0 * s && y <= g.yc + 14.0 * s {
                q.dragging = true;
                SetCapture(hwnd);
                set_manual(x_to_pct(&g, x));
            }
            0
        }
        WM_MOUSEMOVE => {
            if q.dragging {
                let (x, _) = point(lp);
                set_manual(x_to_pct(&geo(s), x));
            }
            0
        }
        WM_LBUTTONUP => {
            if q.dragging {
                q.dragging = false;
                ReleaseCapture();
            }
            0
        }
        WM_MOUSEWHEEL => {
            let d = ((wp >> 16) & 0xFFFF) as i16 as i32;
            wheel(d / 120 * 2);
            0
        }
        WM_KEYDOWN => {
            if !control_ok() {
                if wp as u32 == VK_ESCAPE as u32 {
                    hide();
                }
                return 0;
            }
            let cur = current_pct();
            match wp as u16 {
                VK_LEFT | VK_DOWN => set_manual(cur - 1),
                VK_RIGHT | VK_UP => set_manual(cur + 1),
                VK_PRIOR => set_manual(cur + 10),
                VK_NEXT => set_manual(cur - 10),
                VK_HOME => set_manual(MIN_PCT),
                VK_END => set_manual(MAX_PCT),
                0x41 => set_auto(""), // 'A'
                VK_ESCAPE => hide(),
                _ => {}
            }
            0
        }
        WM_ACTIVATE => {
            if (wp & 0xFFFF) as u32 == WA_INACTIVE {
                hide();
            }
            0
        }
        WM_TIMER => {
            if wp == ID_HOVER {
                let mut pt = POINT { x: 0, y: 0 };
                GetCursorPos(&mut pt);
                let mut r: RECT = zeroed();
                GetWindowRect(hwnd, &mut r);
                let m = (8.0 * s) as i32;
                let in_panel = pt.x >= r.left - m && pt.x <= r.right + m && pt.y >= r.top - m && pt.y <= r.bottom + m;
                let a = q.anchor;
                let in_anchor = pt.x >= a.left && pt.x <= a.right && pt.y >= a.top && pt.y <= a.bottom;
                if in_panel || in_anchor || q.dragging {
                    q.out_ticks = 0;
                } else {
                    q.out_ticks += 1;
                    if q.out_ticks >= 3 && GetForegroundWindow() != hwnd {
                        hide();
                    }
                }
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
