//! Raw bindings to `shim/shim.m`. Keep in sync with `shim/shim.h`.
#![allow(non_camel_case_types)]

use libc::{c_char, c_double, c_void, size_t};

/// See `bugle_mix_fn` in `shim.h`.
pub type bugle_mix_fn =
    extern "C" fn(
        ctx: *mut c_void,
        tracks: *const *const f32,
        n_tracks: i32,
        channels: i32,
        sample_rate: i32,
        out: *mut f32,
        n_out: i32,
        frames: i32,
    ) -> i32;

#[repr(C)]
#[derive(Default)]
pub struct bugle_video_info_t {
    pub width: i32,
    pub height: i32,
    pub nominal_fps: f64,
    pub start: f64,
    pub duration: f64,
    pub stored_samples: i64,
}

#[repr(C)]
pub struct bugle_image_t {
    pub data: *mut u8,
    pub width: i32,
    pub height: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct bugle_display_t {
    pub id: u32,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub scale: f64,
    pub is_main: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct bugle_window_t {
    pub id: u32,
    pub pid: i32,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub layer: i32,
    pub on_screen: i32,
    pub owner: [c_char; 256],
    pub title: [c_char; 512],
    pub alpha: f64,
}

#[repr(C)]
pub struct bugle_ocr_line_t {
    pub text: *mut c_char,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub confidence: f64,
}

pub type bugle_select_callback_t = unsafe extern "C" fn(ctx: *mut c_void, ok: i32, x: f64, y: f64, w: f64, h: f64);

extern "C" {
    pub fn bugle_free(p: *mut c_void);
    pub fn bugle_image_free(img: *mut bugle_image_t);
    pub fn bugle_last_error() -> *mut c_char;

    pub fn bugle_screen_access_preflight() -> i32;
    pub fn bugle_screen_access_request() -> i32;
    pub fn bugle_accessibility_trusted(prompt: i32) -> i32;

    pub fn bugle_list_displays(out: *mut bugle_display_t, max: i32) -> i32;
    pub fn bugle_list_windows(out: *mut bugle_window_t, max: i32, on_screen_only: i32) -> i32;

    pub fn bugle_capture_display(display_id: u32, show_cursor: i32) -> bugle_image_t;
    pub fn bugle_capture_area(x: f64, y: f64, w: f64, h: f64, show_cursor: i32) -> bugle_image_t;
    pub fn bugle_capture_window(window_id: u32, shadow: i32, show_cursor: i32) -> bugle_image_t;

    pub fn bugle_ocr(
        rgba: *const u8,
        len: size_t,
        w: i32,
        h: i32,
        langs_csv: *const c_char,
        accurate: i32,
        lang_correction: i32,
        out_lines: *mut *mut bugle_ocr_line_t,
        out_count: *mut i32,
    ) -> i32;
    pub fn bugle_ocr_free(lines: *mut bugle_ocr_line_t, count: i32);

    pub fn bugle_register_translation(
        ref_rgba: *const u8,
        ref_len: size_t,
        rw: i32,
        rh: i32,
        flt_rgba: *const u8,
        flt_len: size_t,
        fw: i32,
        fh: i32,
        dx: *mut c_double,
        dy: *mut c_double,
    ) -> i32;

    pub fn bugle_post_scroll(x: f64, y: f64, dx_lines: i32, dy_lines: i32) -> i32;
    pub fn bugle_cursor_position(x: *mut f64, y: *mut f64);
    pub fn bugle_mouse_buttons() -> i32;
    pub fn bugle_host_time_now() -> f64;
    pub fn bugle_window_frame(window_id: u32, x: *mut f64, y: *mut f64, w: *mut f64, h: *mut f64) -> i32;
    pub fn bugle_child_exited(pid: i32) -> i32;

    pub fn bugle_clipboard_write_image(png: *const u8, len: size_t) -> i32;
    pub fn bugle_clipboard_write_text(text: *const c_char) -> i32;
    pub fn bugle_clipboard_write_file(path: *const c_char) -> i32;

    pub fn bugle_select_rect_blocking(x: *mut f64, y: *mut f64, w: *mut f64, h: *mut f64) -> i32;
    pub fn bugle_pin_image_blocking(png: *const u8, len: size_t, x: f64, y: f64, w: f64, h: f64, opacity: f64);
    pub fn bugle_select_rect_begin(cb: bugle_select_callback_t, ctx: *mut c_void) -> i32;
    pub fn bugle_select_rect_begin_ex(cb: bugle_select_callback_t, ctx: *mut c_void, allow_switch: i32, timeout_seconds: f64) -> i32;
    pub fn bugle_select_rect_active() -> i32;
    pub fn bugle_select_rect_cancel();
    pub fn bugle_select_rect_switch();
    pub fn bugle_app_deactivate();
    pub fn bugle_key_window_title(buf: *mut c_char, cap: size_t) -> i32;
    pub fn bugle_app_bootstrap();
    pub fn bugle_run_loop_once(seconds: f64);
    pub fn bugle_is_main_thread() -> i32;
    pub fn bugle_pin_image_show(png: *const u8, len: size_t, x: f64, y: f64, w: f64, h: f64, opacity: f64) -> i32;
    pub fn bugle_toast_show(
        png: *const u8,
        len: size_t,
        title: *const c_char,
        subtitle: *const c_char,
        open_path: *const c_char,
        seconds: f64,
    );

    pub fn bugle_export_audio_m4a(in_path: *const c_char, out_path: *const c_char) -> i32;
    pub fn bugle_export_mp4(in_path: *const c_char, out_path: *const c_char) -> i32;
    pub fn bugle_export_mov(in_path: *const c_char, out_path: *const c_char) -> i32;
    pub fn bugle_audio_track_count(path: *const c_char) -> i32;
    pub fn bugle_audio_peaks(path: *const c_char, duration: f64, out: *mut f32, n: i32, cancel: *const bool) -> i32;
    pub fn bugle_mix_audio_tracks(
        in_path: *const c_char,
        out_path: *const c_char,
        separate: i32,
        process: bugle_mix_fn,
        ctx: *mut c_void,
    ) -> i32;
    pub fn bugle_video_track_count(path: *const c_char) -> i32;
    pub fn bugle_fixture_write_tones(
        out_path: *const c_char,
        tracks: i32,
        seconds: f64,
        amplitude: f64,
        overlap: i32,
        delay: f64,
        video: i32,
    ) -> *mut c_char;
    pub fn bugle_media_duration(path: *const c_char) -> f64;
    pub fn bugle_video_frame(path: *const c_char, seconds: f64) -> bugle_image_t;
    pub fn bugle_video_info(path: *const c_char, out: *mut bugle_video_info_t) -> i32;
    pub fn bugle_video_sync_at(path: *const c_char, seconds: f64) -> i32;
    pub fn bugle_export_video(
        in_path: *const c_char,
        out_path: *const c_char,
        start: f64,
        end: f64,
        preset: i32,
        mp4: i32,
        fps: i32,
        timeout: f64,
    ) -> i32;
    pub fn bugle_frames_open(path: *const c_char, tolerance: f64) -> *mut c_void;
    pub fn bugle_frames_at(handle: *mut c_void, seconds: f64) -> bugle_image_t;
    pub fn bugle_frames_set_max_size(handle: *mut c_void, max_w: u32, max_h: u32);
    pub fn bugle_frames_close(handle: *mut c_void);
    pub fn bugle_writer_open(path: *const c_char, w: i32, h: i32, fps: i32, mp4: i32, hevc: i32) -> *mut c_void;
    pub fn bugle_writer_append(handle: *mut c_void, rgba: *const u8, len: size_t, w: i32, h: i32) -> i32;
    pub fn bugle_writer_finish(handle: *mut c_void, timeout: f64) -> i32;
    pub fn bugle_writer_cancel(handle: *mut c_void);
    pub fn bugle_seq_open(path: *const c_char, start: f64, end: f64) -> *mut c_void;
    pub fn bugle_seq_next(handle: *mut c_void, pts: *mut f64) -> bugle_image_t;
    pub fn bugle_seq_close(handle: *mut c_void);
    pub fn bugle_mux_audio(
        video_path: *const c_char,
        audio_path: *const c_char,
        start: f64,
        end: f64,
        out_path: *const c_char,
        mp4: i32,
        timeout: f64,
    ) -> i32;
    pub fn bugle_play_sound(name: *const c_char);
    pub fn bugle_open_path(path: *const c_char);

    pub fn bugle_sck_record_available() -> i32;
    pub fn bugle_sck_record_start(
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        window_id: u32,
        fps: i32,
        retina: i32,
        system_audio: i32,
        microphone: i32,
        show_cursor: i32,
        show_clicks: i32,
        max_seconds: f64,
        out_path: *const c_char,
    ) -> *mut c_void;
    pub fn bugle_sck_record_frames(handle: *mut c_void) -> i64;
    pub fn bugle_sck_record_session_start(handle: *mut c_void) -> f64;
    pub fn bugle_sck_record_warnings(handle: *mut c_void) -> *mut c_char;
    pub fn bugle_sck_record_stop(handle: *mut c_void) -> i32;
}
