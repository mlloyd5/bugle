// Test fixtures for the media helpers. Kept in its own object file: nothing
// outside the tests references these symbols, so the linker drops this
// object from the CLI and app binaries.

#import <Foundation/Foundation.h>
#import <AVFoundation/AVFoundation.h>
#include "shim.h"

static char *fixture_error(NSString *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    NSString *s = [[NSString alloc] initWithFormat:fmt arguments:ap];
    va_end(ap);
    const char *u = s.UTF8String;
    return strdup(u ? u : "fixture error");
}

static BOOL wait_ready(AVAssetWriterInput *in) {
    for (int w = 0; w < 5000 && !in.readyForMoreMediaData; w++) usleep(1000);
    return in.readyForMoreMediaData;
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
        if (video) {
            vin = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeVideo outputSettings:@{
                AVVideoCodecKey: AVVideoCodecTypeH264,
                AVVideoWidthKey: @(vsize),
                AVVideoHeightKey: @(vsize),
            }];
            vin.expectsMediaDataInRealTime = YES;
            adaptor = [AVAssetWriterInputPixelBufferAdaptor assetWriterInputPixelBufferAdaptorWithAssetWriterInput:vin
                sourcePixelBufferAttributes:@{
                    (id)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA),
                    (id)kCVPixelBufferWidthKey: @(vsize),
                    (id)kCVPixelBufferHeightKey: @(vsize),
                }];
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
        // Advance every track chunk by chunk in time order so the writer
        // can interleave them.
        for (int64_t start = 0; start < total && ok; start += chunk) {
            while (ok && vin && vnext < vframes && vnext * 48000 / vfps <= start) {
                CVPixelBufferRef px = NULL;
                ok = CVPixelBufferPoolCreatePixelBuffer(NULL, adaptor.pixelBufferPool, &px) == kCVReturnSuccess;
                if (ok) {
                    CVPixelBufferLockBaseAddress(px, 0);
                    memset(CVPixelBufferGetBaseAddress(px), (int)(vnext * 23 % 256),
                           CVPixelBufferGetBytesPerRow(px) * CVPixelBufferGetHeight(px));
                    CVPixelBufferUnlockBaseAddress(px, 0);
                    ok = wait_ready(vin) && [adaptor appendPixelBuffer:px withPresentationTime:CMTimeMake(vnext, vfps)];
                    CVPixelBufferRelease(px);
                }
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
                ok = ok && wait_ready(inputs[t]) && [inputs[t] appendSampleBuffer:sb];
                if (sb) CFRelease(sb);
                pos[t] += n;
            }
        }
        CFRelease(fmt);
        if (!ok) {
            NSString *why = writer.error.localizedDescription ?: @"append failed";
            [writer cancelWriting];
            return fixture_error(@"writing the tone fixture failed: %@", why);
        }
        for (AVAssetWriterInput *in in inputs) [in markAsFinished];
        [vin markAsFinished];
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        [writer finishWritingWithCompletionHandler:^{ dispatch_semaphore_signal(sem); }];
        dispatch_semaphore_wait(sem, DISPATCH_TIME_FOREVER);
        if (writer.status != AVAssetWriterStatusCompleted) {
            return fixture_error(@"finalising the tone fixture failed: %@", writer.error.localizedDescription);
        }
        return NULL;
    }
}
