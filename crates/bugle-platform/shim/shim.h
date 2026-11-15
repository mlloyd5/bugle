// C ABI exported by shim.m. Mirrored in src/ffi.rs.
#pragma once
#include <stdint.h>
#include <stddef.h>

typedef struct {
    uint8_t *data;   // RGBA8, non-premultiplied, row-major, malloc'd
    int32_t width;
    int32_t height;
} bugle_image_t;

typedef struct {
    uint32_t id;
    double x, y, w, h;   // points, global top-left origin
    double scale;
    int32_t is_main;
} bugle_display_t;

typedef struct {
    uint32_t id;
    int32_t pid;
    double x, y, w, h;
    int32_t layer;
    int32_t on_screen;
    char owner[256];
    char title[512];
} bugle_window_t;

typedef struct {
    char *text;
    double x, y, w, h;   // pixels, top-left origin
    double confidence;
} bugle_ocr_line_t;

void bugle_free(void *p);
void bugle_image_free(bugle_image_t *img);
// The calling thread's last error, handed over (free with bugle_free) and
// cleared; NULL when none. Errors are per thread: read it on the thread that
// made the failing call, before calling into the shim again.
char *bugle_last_error(void);

int32_t bugle_screen_access_preflight(void);
int32_t bugle_screen_access_request(void);
int32_t bugle_accessibility_trusted(int32_t prompt);

int32_t bugle_list_displays(bugle_display_t *out, int32_t max);
// Writes up to `max` windows and returns the total count found (may exceed
// `max`), or -1 on failure. `out` may be NULL when `max` is 0.
int32_t bugle_list_windows(bugle_window_t *out, int32_t max, int32_t on_screen_only);

bugle_image_t bugle_capture_display(uint32_t display_id, int32_t show_cursor);
// Rejects non-finite rects and sides outside (0, 16384] points.
bugle_image_t bugle_capture_area(double x, double y, double w, double h, int32_t show_cursor);
bugle_image_t bugle_capture_window(uint32_t window_id, int32_t shadow, int32_t show_cursor);

// `len` is the byte length of `rgba`; it must be at least w * h * 4.
int32_t bugle_ocr(const uint8_t *rgba, size_t len, int32_t w, int32_t h, const char *langs_csv,
                  int32_t accurate, int32_t lang_correction,
                  bugle_ocr_line_t **out_lines, int32_t *out_count);
void bugle_ocr_free(bugle_ocr_line_t *lines, int32_t count);

int32_t bugle_register_translation(const uint8_t *ref_rgba, size_t ref_len, int32_t rw, int32_t rh,
                                   const uint8_t *flt_rgba, size_t flt_len, int32_t fw, int32_t fh,
                                   double *dx, double *dy);

int32_t bugle_post_scroll(double x, double y, int32_t dx_lines, int32_t dy_lines);
void bugle_cursor_position(double *x, double *y);

int32_t bugle_clipboard_write_image(const uint8_t *png, size_t len);
int32_t bugle_clipboard_write_text(const char *text);
int32_t bugle_clipboard_write_file(const char *path);

int32_t bugle_select_rect_blocking(double *x, double *y, double *w, double *h);
// Non-blocking variant for hosts that already run the AppKit loop: returns 1
// and later calls `cb` on the main thread (ok=0 when cancelled), or 0 when a
// selection is already in progress (cb is then never called).
typedef void (*bugle_select_callback_t)(void *ctx, int32_t ok, double x, double y, double w, double h);
int32_t bugle_select_rect_begin(bugle_select_callback_t cb, void *ctx);
int32_t bugle_select_rect_active(void);
void bugle_select_rect_cancel(void);
void bugle_pin_image_blocking(const uint8_t *png, size_t len, double x, double y, double w, double h, double opacity);
int32_t bugle_pin_image_show(const uint8_t *png, size_t len, double x, double y, double w, double h, double opacity);
void bugle_toast_show(const uint8_t *png, size_t len, const char *title, const char *subtitle, const char *open_path,
                      double seconds);

int32_t bugle_export_audio_m4a(const char *in_path, const char *out_path);
int32_t bugle_export_mp4(const char *in_path, const char *out_path);
int32_t bugle_export_mov(const char *in_path, const char *out_path);
double bugle_media_duration(const char *path);
bugle_image_t bugle_video_frame(const char *path, double seconds);

// Editing. `bugle_export_video` exports [start, end) seconds of a movie with an
// AVAssetExportSession preset (0 passthrough, 1 highest quality H.264, 2 HEVC,
// 3 1920x1080, 4 1280x720, 5 960x540) to .mp4 (`mp4` != 0) or .mov; `fps` > 0
// resamples through a video composition (refused with passthrough). Waits at
// most `timeout` seconds (<= 0: no limit), then cancels the export. The frame
// reader decodes frames at arbitrary times (`tolerance` seconds either side);
// the frame writer encodes RGBA frames (even sizes) to H.264 or HEVC at a
// fixed rate.
typedef struct {
    int32_t width, height;      // display size (pixels, after rotation)
    double nominal_fps;
    double start, duration;     // video track's time range, seconds
    int64_t stored_samples;     // samples in the media (incl. edit-list-hidden ones); -1 unknown
} bugle_video_info_t;
int32_t bugle_video_info(const char *path, bugle_video_info_t *out);
int32_t bugle_video_sync_at(const char *path, double seconds);   // 1 when a cut there needs no re-encode
int32_t bugle_export_video(const char *in_path, const char *out_path, double start, double end, int32_t preset,
                           int32_t mp4, int32_t fps, double timeout);
void *bugle_frames_open(const char *path, double tolerance);
bugle_image_t bugle_frames_at(void *handle, double seconds);
void bugle_frames_close(void *handle);
void *bugle_writer_open(const char *path, int32_t w, int32_t h, int32_t fps, int32_t mp4, int32_t hevc);
int32_t bugle_writer_append(void *handle, const uint8_t *rgba, size_t len, int32_t w, int32_t h);
int32_t bugle_writer_finish(void *handle, double timeout);   // finalises and releases the handle
void bugle_writer_cancel(void *handle);      // discards the file and releases the handle
void bugle_play_sound(const char *name);
void bugle_open_path(const char *path);

// ScreenCaptureKit recorder: H.264 video + AAC system audio (+ AAC microphone
// as a second audio track) written straight to an .mp4. `window_id` 0 records
// the rect; `max_seconds` <= 0 disables the auto-stop. Stop finalises the
// file and releases the handle.
int32_t bugle_sck_record_available(void);
void *bugle_sck_record_start(double x, double y, double w, double h, uint32_t window_id, int32_t fps, int32_t retina,
                             int32_t system_audio, int32_t microphone, int32_t show_cursor, double max_seconds,
                             const char *out_path);
int64_t bugle_sck_record_frames(void *handle);
char *bugle_sck_record_warnings(void *handle);   // malloc'd, NULL when none
int32_t bugle_sck_record_stop(void *handle);
