// Test fixtures for the media helpers. Kept in its own object file: nothing
// outside the tests references these symbols, so the linker drops this
// object from the CLI and app binaries.

#import <Foundation/Foundation.h>
#import <AVFoundation/AVFoundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#include "shim.h"

static char *fixture_error(NSString *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    NSString *s = [[NSString alloc] initWithFormat:fmt arguments:ap];
    va_end(ap);
    const char *u = s.UTF8String;
    return strdup(u ? u : "fixture error");
}

// Wait for `in` to take more data, then make sure the writer is still
// writing: appending to a writer that has failed or stopped raises an
// Objective-C exception, which must not cross into Rust.
static BOOL ready_to_append(AVAssetWriter *writer, AVAssetWriterInput *in, NSString **failure) {
    // A deadline rather than a count: with many encoders busy at once (test
    // binaries run side by side) the input can stay full for seconds.
    uint64_t deadline = clock_gettime_nsec_np(CLOCK_UPTIME_RAW) + 30 * NSEC_PER_SEC;
    while (!in.readyForMoreMediaData && writer.status == AVAssetWriterStatusWriting &&
           clock_gettime_nsec_np(CLOCK_UPTIME_RAW) < deadline) {
        usleep(1000);
    }
    if (writer.status != AVAssetWriterStatusWriting) {
        *failure = writer.error.localizedDescription
                       ?: [NSString stringWithFormat:@"the writer stopped (status %ld)", (long)writer.status];
        return NO;
    }
    if (!in.readyForMoreMediaData) {
        *failure = @"timed out waiting for the writer input";
        return NO;
    }
    return YES;
}

char *bugle_fixture_write_tones(const char *out_path, int32_t tracks, double seconds, double amplitude, int32_t overlap,
                                double delay, int32_t video) {
    @autoreleasepool {
        if (tracks < 1 || tracks > 8 || !(seconds > 0 && seconds <= 60) || !(delay >= 0 && delay < seconds)) {
            return fixture_error(@"bad tone fixture parameters");
        }
        NSURL *out = [NSURL fileURLWithPath:[NSString stringWithUTF8String:out_path]];
        [[NSFileManager defaultManager] removeItemAtURL:out error:nil];
        NSError *err = nil;
        AVAssetWriter *writer = [[AVAssetWriter alloc] initWithURL:out fileType:AVFileTypeMPEG4 error:&err];
        if (!writer) return fixture_error(@"AVAssetWriter: %@", err.localizedDescription);
        // Header first, like the recorder's own files.
        writer.shouldOptimizeForNetworkUse = YES;
        NSDictionary *aac = @{
            AVFormatIDKey: @(kAudioFormatMPEG4AAC),
            AVSampleRateKey: @48000,
            AVNumberOfChannelsKey: @2,
            AVEncoderBitRateKey: @128000,
        };
        NSMutableArray<AVAssetWriterInput *> *inputs = [NSMutableArray array];
        for (int32_t i = 0; i < tracks; i++) {
            AVAssetWriterInput *in = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeAudio outputSettings:aac];
            // Real-time inputs are not held back for interleaving, which a
            // delayed track would otherwise stall.
            in.expectsMediaDataInRealTime = YES;
            if (![writer canAddInput:in]) return fixture_error(@"writer refused audio track %d", i);
            [writer addInput:in];
            [inputs addObject:in];
        }
        const int32_t vfps = 10, vsize = 64;
        AVAssetWriterInput *vin = nil;
        AVAssetWriterInputPixelBufferAdaptor *adaptor = nil;
        // Frames are allocated one by one rather than from the adaptor's
        // pixelBufferPool. That pool is owned by the VideoToolbox compression
        // session, which may replace it at any time: a pointer read from the
        // adaptor can dangle by the time CoreVideo uses it, and it then traps
        // or faults and takes the test binary down. A pool buys nothing for a
        // few 64x64 frames.
        NSDictionary *pxattrs = @{
            (id)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA),
            (id)kCVPixelBufferWidthKey: @(vsize),
            (id)kCVPixelBufferHeightKey: @(vsize),
            (id)kCVPixelBufferIOSurfacePropertiesKey: @{},
        };
        if (video) {
            vin = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeVideo outputSettings:@{
                AVVideoCodecKey: AVVideoCodecTypeH264,
                AVVideoWidthKey: @(vsize),
                AVVideoHeightKey: @(vsize),
            }];
            vin.expectsMediaDataInRealTime = YES;
            adaptor = [AVAssetWriterInputPixelBufferAdaptor assetWriterInputPixelBufferAdaptorWithAssetWriterInput:vin
                sourcePixelBufferAttributes:pxattrs];
            if (![writer canAddInput:vin]) return fixture_error(@"writer refused the video track");
            [writer addInput:vin];
        }
        AudioStreamBasicDescription asbd = {0};
        asbd.mSampleRate = 48000;
        asbd.mFormatID = kAudioFormatLinearPCM;
        asbd.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked;
        asbd.mChannelsPerFrame = 2;
        asbd.mBitsPerChannel = 32;
        asbd.mBytesPerFrame = 8;
        asbd.mFramesPerPacket = 1;
        asbd.mBytesPerPacket = 8;
        CMAudioFormatDescriptionRef fmt = NULL;
        if (CMAudioFormatDescriptionCreate(kCFAllocatorDefault, &asbd, 0, NULL, 0, NULL, NULL, &fmt) != noErr) {
            return fixture_error(@"could not describe the PCM format");
        }
        if (![writer startWriting]) {
            CFRelease(fmt);
            return fixture_error(@"could not start writing: %@", writer.error.localizedDescription);
        }
        [writer startSessionAtSourceTime:kCMTimeZero];
        enum { chunk = 1024 };
        const int64_t total = (int64_t)(seconds * 48000);
        const int64_t slice = total / tracks;
        int64_t pos[8];
        for (int32_t t = 0; t < tracks; t++) pos[t] = t == 0 ? 0 : (int64_t)(delay * 48000);
        const int64_t vframes = (int64_t)(seconds * vfps);
        int64_t vnext = 0;
        float buf[chunk * 2];
        BOOL ok = YES;
        NSString *failure = nil;
        // Advance every track chunk by chunk in time order so the writer
        // can interleave them.
        for (int64_t start = 0; start < total && ok; start += chunk) {
            while (ok && vin && vnext < vframes && vnext * 48000 / vfps <= start) {
                CVPixelBufferRef px = NULL;
                CVReturn cr = CVPixelBufferCreate(kCFAllocatorDefault, vsize, vsize, kCVPixelFormatType_32BGRA,
                                                  (__bridge CFDictionaryRef)pxattrs, &px);
                if (cr != kCVReturnSuccess || !px) {
                    ok = NO;
                    failure = [NSString stringWithFormat:@"could not allocate a pixel buffer for a video frame (CVReturn %d)", cr];
                    break;
                }
                cr = CVPixelBufferLockBaseAddress(px, 0);
                if (cr == kCVReturnSuccess) {
                    memset(CVPixelBufferGetBaseAddress(px), (int)(vnext * 23 % 256),
                           CVPixelBufferGetBytesPerRow(px) * CVPixelBufferGetHeight(px));
                    CVPixelBufferUnlockBaseAddress(px, 0);
                    ok = ready_to_append(writer, vin, &failure) &&
                         [adaptor appendPixelBuffer:px withPresentationTime:CMTimeMake(vnext, vfps)];
                } else {
                    ok = NO;
                    failure = [NSString stringWithFormat:@"could not lock a video frame (CVReturn %d)", cr];
                }
                CVPixelBufferRelease(px);
                vnext++;
            }
            for (int32_t t = 0; t < tracks && ok; t++) {
                if (pos[t] >= start + chunk) continue;
                int n = (int)MIN((int64_t)chunk, total - pos[t]);
                if (n <= 0) continue;
                double freq = overlap ? 440.0 : 440.0 * (t + 1);
                for (int k = 0; k < n; k++) {
                    int64_t f = pos[t] + k;
                    BOOL on = overlap || (f >= t * slice && f < (t + 1) * slice);
                    float v = on ? (float)(amplitude * sin(2 * M_PI * freq * (double)f / 48000.0)) : 0.0f;
                    buf[2 * k] = buf[2 * k + 1] = v;
                }
                size_t bytes = (size_t)n * 8;
                CMBlockBufferRef bb = NULL;
                CMSampleBufferRef sb = NULL;
                ok = CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, NULL, bytes, kCFAllocatorDefault, NULL, 0,
                                                        bytes, kCMBlockBufferAssureMemoryNowFlag, &bb) == noErr &&
                     CMBlockBufferReplaceDataBytes(buf, bb, 0, bytes) == noErr &&
                     CMAudioSampleBufferCreateReadyWithPacketDescriptions(kCFAllocatorDefault, bb, fmt, n,
                                                                          CMTimeMake(pos[t], 48000), NULL, &sb) == noErr;
                if (bb) CFRelease(bb);
                ok = ok && ready_to_append(writer, inputs[t], &failure) && [inputs[t] appendSampleBuffer:sb];
                if (sb) CFRelease(sb);
                pos[t] += n;
            }
        }
        CFRelease(fmt);
        if (!ok) {
            NSString *why = failure ?: writer.error.localizedDescription ?: @"append failed";
            [writer cancelWriting];
            return fixture_error(@"writing the tone fixture failed: %@", why);
        }
        for (AVAssetWriterInput *in in inputs) [in markAsFinished];
        [vin markAsFinished];
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        [writer finishWritingWithCompletionHandler:^{ dispatch_semaphore_signal(sem); }];
        if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 30 * NSEC_PER_SEC)) != 0) {
            return fixture_error(@"finalising the tone fixture timed out after 30 s");
        }
        if (writer.status != AVAssetWriterStatusCompleted) {
            return fixture_error(@"finalising the tone fixture failed: %@", writer.error.localizedDescription);
        }
        return NULL;
    }
}

int32_t bugle_fixture_audio_level(int32_t format, int32_t channels, int32_t interleaved, const float *samples,
                                  int32_t frames, double rate, double *out) {
    if (format < 0 || format > 4 || channels < 1 || channels > 8 || frames < 1 || !(rate > 0) || !samples || !out) return -1;
    BOOL isFloat = format == 0 || format == 2;
    UInt32 bps = format == 1 || format == 4 ? 2 : format == 2 ? 8 : 4;
    AudioStreamBasicDescription asbd = {0};
    asbd.mSampleRate = rate;
    asbd.mFormatID = kAudioFormatLinearPCM;
    asbd.mFormatFlags = (isFloat ? kAudioFormatFlagIsFloat : kAudioFormatFlagIsSignedInteger) | kAudioFormatFlagIsPacked |
                        (interleaved ? 0 : kAudioFormatFlagIsNonInterleaved) | (format == 4 ? kAudioFormatFlagIsBigEndian : 0);
    asbd.mBitsPerChannel = bps * 8;
    asbd.mChannelsPerFrame = (UInt32)channels;
    asbd.mFramesPerPacket = 1;
    asbd.mBytesPerFrame = interleaved ? bps * (UInt32)channels : bps;
    asbd.mBytesPerPacket = asbd.mBytesPerFrame;
    UInt32 nbuf = interleaved ? 1 : (UInt32)channels;
    size_t bytes = (size_t)frames * channels * bps;
    uint8_t *data = calloc(1, bytes);
    AudioBufferList *abl = calloc(1, offsetof(AudioBufferList, mBuffers) + sizeof(AudioBuffer) * nbuf);
    if (!data || !abl) { free(data); free(abl); return -1; }
    for (int32_t c = 0; c < channels; c++) {
        for (int32_t i = 0; i < frames; i++) {
            float v = samples[(size_t)c * frames + i];
            size_t k = interleaved ? (size_t)i * channels + c : (size_t)c * frames + i;
            double x = fmax(-1.0, fmin(v, 1.0));
            switch (format) {
                case 0: ((float *)data)[k] = v; break;
                case 1: case 4: ((int16_t *)data)[k] = (int16_t)lrint(fmin(x * 32768.0, 32767.0)); break;
                case 2: ((double *)data)[k] = v; break;
                case 3: ((int32_t *)data)[k] = (int32_t)llrint(fmin(x * 2147483648.0, 2147483647.0)); break;
            }
        }
    }
    abl->mNumberBuffers = nbuf;
    for (UInt32 b = 0; b < nbuf; b++) {
        abl->mBuffers[b].mNumberChannels = interleaved ? (UInt32)channels : 1;
        abl->mBuffers[b].mDataByteSize = (UInt32)(bytes / nbuf);
        abl->mBuffers[b].mData = data + b * (bytes / nbuf);
    }
    int32_t rc = -1;
    CMAudioFormatDescriptionRef fmt = NULL;
    CMSampleBufferRef sb = NULL;
    if (CMAudioFormatDescriptionCreate(kCFAllocatorDefault, &asbd, 0, NULL, 0, NULL, NULL, &fmt) == noErr &&
        CMAudioSampleBufferCreateWithPacketDescriptions(kCFAllocatorDefault, NULL, false, NULL, NULL, fmt, frames,
                                                        CMTimeMake(0, (int32_t)rate), NULL, &sb) == noErr &&
        CMSampleBufferSetDataBufferFromAudioBufferList(sb, kCFAllocatorDefault, kCFAllocatorDefault, 0, abl) == noErr) {
        double peak, ss, seconds;
        size_t n;
        if (bugle_audio_buffer_stats(sb, &peak, &ss, &n, &seconds) && n > 0) {
            out[0] = peak;
            out[1] = sqrt(ss / (double)n);
            out[2] = seconds;
            rc = 0;
        }
    }
    if (sb) CFRelease(sb);
    if (fmt) CFRelease(fmt);
    free(abl);
    free(data);
    return rc;
}

// The stream recorder (shim.m), as far as the fixtures below drive it.
@interface BugleSckRecorder : NSObject
@property (nonatomic, strong) AVAssetWriter *writer;
@property (atomic) double stoppedAt;
- (void)stream:(id)stream didStopWithError:(NSError *)error;
- (void)stream:(id)stream didOutputSampleBuffer:(CMSampleBufferRef)sb ofType:(NSInteger)type;
- (void)stopCapture;
@end

void *bugle_fixture_sck_recorder(void) {
    return (__bridge_retained void *)[[BugleSckRecorder alloc] init];
}

void bugle_fixture_sck_stream_failed(void *handle, const char *message, int32_t user_stopped) {
    if (!handle) return;
    @autoreleasepool {
        BugleSckRecorder *rec = (__bridge BugleSckRecorder *)handle;
        NSString *m = [NSString stringWithUTF8String:message ? message : ""] ?: @"";
        NSError *e = user_stopped ? [NSError errorWithDomain:SCStreamErrorDomain code:SCStreamErrorUserStopped userInfo:nil]
                                  : [NSError errorWithDomain:@"BugleFixture" code:-3801 userInfo:@{NSLocalizedDescriptionKey: m}];
        [rec stream:nil didStopWithError:e];
    }
}

char *bugle_fixture_sck_writer_failed(void *handle) {
    if (!handle) return fixture_error(@"no recorder");
    @autoreleasepool {
        BugleSckRecorder *rec = (__bridge BugleSckRecorder *)handle;
        // A writer whose folder does not exist fails as it starts.
        NSString *dir = [NSTemporaryDirectory() stringByAppendingPathComponent:[[NSUUID UUID] UUIDString]];
        NSURL *url = [NSURL fileURLWithPath:[dir stringByAppendingPathComponent:@"x.mp4"]];
        NSError *err = nil;
        AVAssetWriter *writer = [[AVAssetWriter alloc] initWithURL:url fileType:AVFileTypeMPEG4 error:&err];
        if (!writer) return fixture_error(@"AVAssetWriter: %@", err.localizedDescription);
        AVAssetWriterInput *in = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeVideo outputSettings:@{
            AVVideoCodecKey: AVVideoCodecTypeH264, AVVideoWidthKey: @64, AVVideoHeightKey: @64,
        }];
        in.expectsMediaDataInRealTime = YES;
        if ([writer canAddInput:in]) [writer addInput:in];
        if ([writer startWriting] || writer.status != AVAssetWriterStatusFailed) {
            [writer cancelWriting];
            return fixture_error(@"the writer did not fail (status %ld)", (long)writer.status);
        }
        rec.writer = writer;
        // An empty buffer with no image, as an idle frame of a static screen.
        CMSampleBufferRef sb = NULL;
        if (CMSampleBufferCreate(kCFAllocatorDefault, NULL, true, NULL, NULL, NULL, 0, 0, NULL, 0, NULL, &sb) != noErr || !sb) {
            return fixture_error(@"could not create a sample buffer");
        }
        [rec stream:nil didOutputSampleBuffer:sb ofType:SCStreamOutputTypeScreen];
        CFRelease(sb);
        return NULL;
    }
}

void bugle_fixture_sck_stop_capture(void *handle) {
    if (handle) [(__bridge BugleSckRecorder *)handle stopCapture];
}

double bugle_fixture_sck_stopped_at(void *handle) {
    return handle ? ((__bridge BugleSckRecorder *)handle).stoppedAt : NAN;
}
