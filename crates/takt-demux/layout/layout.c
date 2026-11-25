// SPDX-License-Identifier: GPL-3.0-or-later
// Prints the offsets src/bin/takt-demux-helper/ffi.rs asserts, from the headers of the FFmpeg at hand. How to run it: see README.md here.

#include <stddef.h>
#include <stdio.h>
#include <libavcodec/codec_par.h>
#include <libavcodec/packet.h>
#include <libavformat/avformat.h>

#define O(type, field) printf("%-20s %-22s %zu\n", #type, #field, offsetof(type, field))

int main(void) {
    printf("avformat %d avcodec %d avutil %d\n", LIBAVFORMAT_VERSION_MAJOR, LIBAVCODEC_VERSION_MAJOR,
           LIBAVUTIL_VERSION_MAJOR);
    O(AVFormatContext, iformat);
    O(AVIOContext, buffer);
    O(AVFormatContext, pb);
    O(AVFormatContext, nb_streams);
    O(AVFormatContext, streams);
    O(AVFormatContext, start_time);
    O(AVFormatContext, duration);
    O(AVInputFormat, name);
    O(AVStream, index);
    O(AVStream, codecpar);
    O(AVStream, time_base);
    O(AVStream, start_time);
    O(AVStream, duration);
    O(AVStream, nb_frames);
    O(AVStream, disposition);
    O(AVStream, sample_aspect_ratio);
    O(AVStream, metadata);
    O(AVStream, avg_frame_rate);
    O(AVCodecParameters, codec_type);
    O(AVCodecParameters, codec_id);
    O(AVCodecParameters, codec_tag);
    O(AVCodecParameters, extradata);
    O(AVCodecParameters, extradata_size);
    O(AVCodecParameters, format);
    O(AVCodecParameters, bit_rate);
    O(AVCodecParameters, bits_per_coded_sample);
    O(AVCodecParameters, profile);
    O(AVCodecParameters, level);
    O(AVCodecParameters, width);
    O(AVCodecParameters, height);
    O(AVCodecParameters, sample_aspect_ratio);
    O(AVCodecParameters, framerate);
    O(AVCodecParameters, field_order);
    O(AVCodecParameters, color_range);
    O(AVCodecParameters, color_primaries);
    O(AVCodecParameters, color_trc);
    O(AVCodecParameters, color_space);
    O(AVCodecParameters, chroma_location);
    O(AVCodecParameters, video_delay);
    O(AVCodecParameters, ch_layout);
    O(AVCodecParameters, sample_rate);
    O(AVCodecParameters, block_align);
    O(AVCodecParameters, frame_size);
    O(AVCodecParameters, initial_padding);
    O(AVCodecParameters, trailing_padding);
    O(AVCodecParameters, seek_preroll);
    O(AVChannelLayout, order);
    O(AVChannelLayout, nb_channels);
    O(AVChannelLayout, u);
    O(AVPacket, pts);
    O(AVPacket, dts);
    O(AVPacket, data);
    O(AVPacket, size);
    O(AVPacket, stream_index);
    O(AVPacket, flags);
    O(AVPacket, duration);
    O(AVPacket, pos);
    printf("AVMEDIA video %d audio %d data %d subtitle %d attachment %d\n", AVMEDIA_TYPE_VIDEO, AVMEDIA_TYPE_AUDIO,
           AVMEDIA_TYPE_DATA, AVMEDIA_TYPE_SUBTITLE, AVMEDIA_TYPE_ATTACHMENT);
    printf("AV_CHANNEL_ORDER_NATIVE %d AVERROR_EOF %d AVERROR(EAGAIN) %d AV_LOG_QUIET %d AVIO_SEEKABLE_NORMAL %d\n",
           AV_CHANNEL_ORDER_NATIVE, AVERROR_EOF, AVERROR(EAGAIN), AV_LOG_QUIET, AVIO_SEEKABLE_NORMAL);
    return 0;
}
