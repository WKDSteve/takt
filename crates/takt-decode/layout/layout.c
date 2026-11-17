// SPDX-License-Identifier: GPL-3.0-or-later
// Prints the offsets src/bin/takt-decode-helper/ffi.rs asserts, from the headers of the FFmpeg at hand. How to run it: see README.md here.
#include <stddef.h>
#include <stdio.h>
#include <libavcodec/avcodec.h>
#include <libavcodec/codec_par.h>
#include <libavcodec/packet.h>
#include <libavutil/frame.h>
#include <libavutil/opt.h>

#define O(type, field) printf("%-20s %-22s %zu\n", #type, #field, offsetof(type, field))

int main(void) {
    printf("avcodec %d avutil %d\n", LIBAVCODEC_VERSION_MAJOR, LIBAVUTIL_VERSION_MAJOR);
    O(AVFrame, data);
    O(AVFrame, linesize);
    O(AVFrame, width);
    O(AVFrame, height);
    O(AVFrame, format);
    O(AVFrame, pts);
    O(AVFrame, flags);
    O(AVFrame, color_range);
    O(AVFrame, color_primaries);
    O(AVFrame, color_trc);
    O(AVFrame, colorspace);
    O(AVFrame, chroma_location);
    O(AVFrame, best_effort_timestamp);
    O(AVCodecParameters, codec_type);
    O(AVCodecParameters, codec_id);
    O(AVCodecParameters, codec_tag);
    O(AVCodecParameters, extradata);
    O(AVCodecParameters, extradata_size);
    O(AVCodecParameters, format);
    O(AVCodecParameters, profile);
    O(AVCodecParameters, level);
    O(AVCodecParameters, width);
    O(AVCodecParameters, height);
    O(AVCodecParameters, color_range);
    O(AVCodecParameters, color_primaries);
    O(AVCodecParameters, color_trc);
    O(AVCodecParameters, color_space);
    O(AVCodecParameters, chroma_location);
    O(AVPacket, pts);
    O(AVPacket, dts);
    O(AVPacket, data);
    O(AVPacket, size);
    O(AVPacket, flags);
    O(AVPacket, duration);
    O(AVCodec, name);
    O(AVCodec, type);
    O(AVCodec, id);
    printf("AVMEDIA_TYPE_VIDEO %d AVERROR_EOF %d AVERROR(EAGAIN) %d AV_LOG_QUIET %d\n", AVMEDIA_TYPE_VIDEO, AVERROR_EOF,
           AVERROR(EAGAIN), AV_LOG_QUIET);
    printf("AV_PKT_FLAG_KEY %d AV_PKT_FLAG_CORRUPT %d AV_PKT_FLAG_DISCARD %d AV_FRAME_FLAG_KEY %d AV_FRAME_FLAG_CORRUPT %d\n",
           AV_PKT_FLAG_KEY, AV_PKT_FLAG_CORRUPT, AV_PKT_FLAG_DISCARD, AV_FRAME_FLAG_KEY, AV_FRAME_FLAG_CORRUPT);
    printf("AV_INPUT_BUFFER_PADDING_SIZE %d AV_NUM_DATA_POINTERS %d AV_CODEC_FLAG_OUTPUT_CORRUPT %d AV_OPT_SEARCH_CHILDREN %d\n",
           AV_INPUT_BUFFER_PADDING_SIZE, AV_NUM_DATA_POINTERS, AV_CODEC_FLAG_OUTPUT_CORRUPT, AV_OPT_SEARCH_CHILDREN);
    return 0;
}
