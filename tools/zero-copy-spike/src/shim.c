// SPDX-License-Identifier: GPL-3.0-or-later
// The FFmpeg side of the spike: demux and hardware decode, and what it takes to get the picture onto the
// D3D12 device of the caller. Two ways:
//   D3D12VA: FFmpeg decodes on the caller's device; the decoder texture itself is handed out (no copy).
//   D3D11VA: FFmpeg decodes on a D3D11 device of the same adapter; each picture is copied on the GPU into a
//            shared texture that the D3D12 device opened by handle (one GPU copy, no CPU copy).
// C, because the layouts of AVCodecContext and AVFrame belong to the headers and not to a hand-written
// Rust mirror.
#define COBJMACROS
#include <initguid.h>

#include <d3d11_4.h>
#include <d3d12.h>
#include <dxgi1_4.h>
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_d3d11va.h>
#include <libavutil/hwcontext_d3d12va.h>
#include <libavutil/pixdesc.h>
#include <stdio.h>
#include <string.h>

#define SHARED_COUNT 4

// The D3D11 side of the second way and its bridge to D3D12.
typedef struct Bridge {
    ID3D12Device *dev12;
    AVD3D11VADeviceContext *hw;
    ID3D11DeviceContext4 *ctx4;
    ID3D11Fence *fence11;
    ID3D12Fence *fence12;
    uint64_t fence_value;
    ID3D11Texture2D *tex11[SHARED_COUNT];
    ID3D12Resource *tex12[SHARED_COUNT];
    int next;
} Bridge;

typedef struct Spike {
    AVFormatContext *fmt;
    AVCodecContext *dec;
    AVPacket *pkt;
    AVFrame *sw;
    int stream;
    int draining;
    int d3d11;
    Bridge bridge;
} Spike;

typedef struct SpikeFrame {
    void *frame;          // AVFrame*, owned by the caller until spike_frame_free
    void *texture;        // ID3D12Resource*, borrowed: from the frame (D3D12VA) or from the Spike (D3D11VA)
    void *fence;          // ID3D12Fence*, borrowed likewise
    uint64_t fence_value; // the texture holds the picture when the fence reaches this
    int subresource;      // array slice in texture-array mode, else 0
    int array_size;       // DepthOrArraySize of the resource
    int width, height;         // the visible picture
    int res_width, res_height; // the resource, which may be larger (coded size)
    int ten_bit;               // 0: NV12, 1: P010
    unsigned res_flags;        // D3D12_RESOURCE_FLAGS of the resource
    int64_t pts;
} SpikeFrame;

typedef struct SpikePlanes {
    const uint8_t *data[2];
    int linesize[2];
} SpikePlanes;

static enum AVPixelFormat pick_hw(AVCodecContext *ctx, const enum AVPixelFormat *fmts) {
    const Spike *s = ctx->opaque;
    enum AVPixelFormat want = s->d3d11 ? AV_PIX_FMT_D3D11 : AV_PIX_FMT_D3D12;
    for (const enum AVPixelFormat *p = fmts; *p != AV_PIX_FMT_NONE; p++)
        if (*p == want)
            return *p;
    return AV_PIX_FMT_NONE; // no silent software fallback: the spike measures the hardware path
}

static void bridge_close(Bridge *b) {
    for (int i = 0; i < SHARED_COUNT; i++) {
        if (b->tex12[i])
            ID3D12Resource_Release(b->tex12[i]);
        if (b->tex11[i])
            ID3D11Texture2D_Release(b->tex11[i]);
    }
    if (b->fence12)
        ID3D12Fence_Release(b->fence12);
    if (b->fence11)
        ID3D11Fence_Release(b->fence11);
    if (b->ctx4)
        ID3D11DeviceContext4_Release(b->ctx4);
    memset(b, 0, sizeof *b);
}

void spike_close(Spike *s) {
    if (!s)
        return;
    av_packet_free(&s->pkt);
    av_frame_free(&s->sw);
    bridge_close(&s->bridge); // before the codec context: it borrows the D3D11 device context
    avcodec_free_context(&s->dec);
    avformat_close_input(&s->fmt);
    av_free(s);
}

static void *fail(Spike *s, char *err, int errlen, const char *what, int code) {
    char buf[128] = "";
    if (code)
        av_strerror(code, buf, sizeof buf);
    snprintf(err, (size_t)errlen, "%s%s%s", what, code ? ": " : "", buf);
    spike_close(s);
    return NULL;
}

// A D3D11 device on the adapter of the D3D12 device, with a fence both devices can see.
static const char *bridge_open(Bridge *b, ID3D12Device *dev12, ID3D11Device **out) {
    LUID luid;
    IDXGIFactory4 *factory = NULL;
    IDXGIAdapter *adapter = NULL;
    ID3D11Device5 *dev5 = NULL;
    ID3D11DeviceContext *ctx = NULL;
    HANDLE handle = NULL;
    const char *what = NULL;
    b->dev12 = dev12;
    ID3D12Device_GetAdapterLuid(dev12, &luid);
    if (FAILED(CreateDXGIFactory1(&IID_IDXGIFactory4, (void **)&factory)))
        return "CreateDXGIFactory1";
    if (FAILED(IDXGIFactory4_EnumAdapterByLuid(factory, luid, &IID_IDXGIAdapter, (void **)&adapter)))
        what = "EnumAdapterByLuid";
    else if (FAILED(D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, NULL, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                                      NULL, 0, D3D11_SDK_VERSION, out, NULL, &ctx)))
        what = "D3D11CreateDevice";
    else if (FAILED(ID3D11Device_QueryInterface(*out, &IID_ID3D11Device5, (void **)&dev5)) ||
             FAILED(ID3D11DeviceContext_QueryInterface(ctx, &IID_ID3D11DeviceContext4, (void **)&b->ctx4)))
        what = "ID3D11Device5/ID3D11DeviceContext4 (needs Windows 10 1703)";
    else if (FAILED(ID3D11Device5_CreateFence(dev5, 0, D3D11_FENCE_FLAG_SHARED, &IID_ID3D11Fence,
                                              (void **)&b->fence11)))
        what = "ID3D11Device5::CreateFence";
    else if (FAILED(ID3D11Fence_CreateSharedHandle(b->fence11, NULL, GENERIC_ALL, NULL, &handle)))
        what = "ID3D11Fence::CreateSharedHandle";
    else if (FAILED(ID3D12Device_OpenSharedHandle(dev12, handle, &IID_ID3D12Fence, (void **)&b->fence12)))
        what = "ID3D12Device::OpenSharedHandle(fence)";
    if (handle)
        CloseHandle(handle);
    if (dev5)
        ID3D11Device5_Release(dev5);
    if (ctx)
        ID3D11DeviceContext_Release(ctx);
    if (adapter)
        IDXGIAdapter_Release(adapter);
    IDXGIFactory4_Release(factory);
    return what;
}

// The shared textures: created on D3D11, opened on D3D12. Both sides see the same memory.
static int bridge_textures(Bridge *b, ID3D11Device *dev11, int width, int height, DXGI_FORMAT format) {
    D3D11_TEXTURE2D_DESC desc = {
        .Width = (UINT)width,
        .Height = (UINT)height,
        .MipLevels = 1,
        .ArraySize = 1,
        .Format = format,
        .SampleDesc = {1, 0},
        .Usage = D3D11_USAGE_DEFAULT,
        .BindFlags = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET,
        .MiscFlags = D3D11_RESOURCE_MISC_SHARED | D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    };
    for (int i = 0; i < SHARED_COUNT; i++) {
        IDXGIResource1 *res = NULL;
        HANDLE handle = NULL;
        HRESULT hr = ID3D11Device_CreateTexture2D(dev11, &desc, NULL, &b->tex11[i]);
        if (SUCCEEDED(hr))
            hr = ID3D11Texture2D_QueryInterface(b->tex11[i], &IID_IDXGIResource1, (void **)&res);
        if (SUCCEEDED(hr))
            hr = IDXGIResource1_CreateSharedHandle(
                res, NULL, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE, NULL, &handle);
        if (SUCCEEDED(hr))
            hr = ID3D12Device_OpenSharedHandle(b->dev12, handle, &IID_ID3D12Resource, (void **)&b->tex12[i]);
        if (handle)
            CloseHandle(handle);
        if (res)
            IDXGIResource1_Release(res);
        if (FAILED(hr)) {
            av_log(NULL, AV_LOG_ERROR, "shared texture %d: HRESULT 0x%08lx\n", i, (unsigned long)hr);
            return AVERROR_EXTERNAL;
        }
    }
    return 0;
}

static AVBufferRef *device_d3d12(ID3D12Device *dev12, int *r) {
    AVBufferRef *dev = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D12VA);
    if (!dev)
        return NULL;
    AVD3D12VADeviceContext *hw = ((AVHWDeviceContext *)dev->data)->hwctx;
    hw->device = dev12;
    ID3D12Device_AddRef(hw->device); // FFmpeg releases it with the context
    if ((*r = av_hwdevice_ctx_init(dev)) < 0)
        av_buffer_unref(&dev);
    return dev;
}

static AVBufferRef *device_d3d11(Bridge *b, ID3D12Device *dev12, const char **what, int *r) {
    AVBufferRef *dev = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA);
    if (!dev)
        return NULL;
    AVD3D11VADeviceContext *hw = ((AVHWDeviceContext *)dev->data)->hwctx;
    if ((*what = bridge_open(b, dev12, &hw->device)) != NULL) { // FFmpeg owns the device from here
        av_buffer_unref(&dev);
        return NULL;
    }
    if ((*r = av_hwdevice_ctx_init(dev)) < 0) {
        av_buffer_unref(&dev);
        return NULL;
    }
    b->hw = hw;
    return dev;
}

// `d3d12_device` is an ID3D12Device* of the caller; the shim takes its own references.
Spike *spike_open(const char *path, void *d3d12_device, int d3d11, int extra_frames, char *err, int errlen) {
    Spike *s = av_mallocz(sizeof *s);
    if (!s)
        return fail(NULL, err, errlen, "out of memory", 0);
    s->d3d11 = d3d11;
    int r = avformat_open_input(&s->fmt, path, NULL, NULL);
    if (r < 0)
        return fail(s, err, errlen, "avformat_open_input", r);
    if ((r = avformat_find_stream_info(s->fmt, NULL)) < 0)
        return fail(s, err, errlen, "avformat_find_stream_info", r);
    s->stream = av_find_best_stream(s->fmt, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    if (s->stream < 0)
        return fail(s, err, errlen, "no video stream", s->stream);
    // The default decoder of a codec may be a software-only wrapper (libdav1d); take the one with the hwaccel.
    enum AVHWDeviceType type = d3d11 ? AV_HWDEVICE_TYPE_D3D11VA : AV_HWDEVICE_TYPE_D3D12VA;
    enum AVCodecID id = s->fmt->streams[s->stream]->codecpar->codec_id;
    const AVCodec *codec = NULL, *c;
    void *it = NULL;
    while (!codec && (c = av_codec_iterate(&it))) {
        if (!av_codec_is_decoder(c) || c->id != id)
            continue;
        const AVCodecHWConfig *cfg;
        for (int i = 0; !codec && (cfg = avcodec_get_hw_config(c, i)); i++)
            if (cfg->device_type == type && (cfg->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX))
                codec = c;
    }
    if (!codec)
        return fail(s, err, errlen, "no decoder with this hwaccel for the codec", 0);
    s->dec = avcodec_alloc_context3(codec);
    if (!s->dec)
        return fail(s, err, errlen, "out of memory", 0);
    if ((r = avcodec_parameters_to_context(s->dec, s->fmt->streams[s->stream]->codecpar)) < 0)
        return fail(s, err, errlen, "avcodec_parameters_to_context", r);

    const char *what = NULL;
    r = 0;
    AVBufferRef *dev = d3d11 ? device_d3d11(&s->bridge, d3d12_device, &what, &r) : device_d3d12(d3d12_device, &r);
    if (!dev)
        return fail(s, err, errlen, what ? what : "hardware device context", r);
    s->dec->hw_device_ctx = dev;
    s->dec->opaque = s;
    s->dec->get_format = pick_hw;
    s->dec->extra_hw_frames = extra_frames; // frames the renderer holds beyond the reference pictures
    if ((r = avcodec_open2(s->dec, codec, NULL)) < 0)
        return fail(s, err, errlen, "avcodec_open2", r);
    s->pkt = av_packet_alloc();
    s->sw = av_frame_alloc();
    if (!s->pkt || !s->sw)
        return fail(s, err, errlen, "out of memory", 0);
    return s;
}

const char *spike_decoder_name(const Spike *s) { return s->dec->codec->name; }

// The next decoded frame of the video stream. 0: a frame, 1: end of stream, <0: AVERROR.
static int receive(Spike *s, AVFrame *f) {
    for (;;) {
        int r = avcodec_receive_frame(s->dec, f);
        if (r == 0)
            return 0;
        if (r == AVERROR_EOF || (r == AVERROR(EAGAIN) && s->draining))
            return 1;
        if (r != AVERROR(EAGAIN))
            return r;
        for (;;) {
            r = av_read_frame(s->fmt, s->pkt);
            if (r < 0) {
                s->draining = 1;
                avcodec_send_packet(s->dec, NULL);
                break;
            }
            if (s->pkt->stream_index != s->stream) {
                av_packet_unref(s->pkt);
                continue;
            }
            r = avcodec_send_packet(s->dec, s->pkt);
            av_packet_unref(s->pkt);
            if (r < 0)
                return r;
            break;
        }
    }
}

// D3D11VA: copies the picture on the GPU into the next shared texture and signals the shared fence.
static int bridge_copy(Spike *s, const AVFrame *f, int ten_bit, SpikeFrame *out) {
    Bridge *b = &s->bridge;
    if (!b->tex11[0]) {
        int r = bridge_textures(b, b->hw->device, f->width, f->height,
                                ten_bit ? DXGI_FORMAT_P010 : DXGI_FORMAT_NV12);
        if (r < 0)
            return r;
    }
    int i = b->next;
    b->next = (b->next + 1) % SHARED_COUNT;
    D3D11_BOX box = {0, 0, 0, (UINT)f->width, (UINT)f->height, 1};
    b->hw->lock(b->hw->lock_ctx);
    ID3D11DeviceContext4_CopySubresourceRegion(b->ctx4, (ID3D11Resource *)b->tex11[i], 0, 0, 0, 0,
                                               (ID3D11Resource *)f->data[0], (UINT)(intptr_t)f->data[1], &box);
    ID3D11DeviceContext4_Signal(b->ctx4, b->fence11, ++b->fence_value);
    ID3D11DeviceContext4_Flush(b->ctx4);
    b->hw->unlock(b->hw->lock_ctx);
    D3D12_RESOURCE_DESC desc;
    ID3D12Resource_GetDesc(b->tex12[i], &desc);
    out->texture = b->tex12[i];
    out->fence = b->fence12;
    out->fence_value = b->fence_value;
    out->subresource = 0;
    out->array_size = 1;
    out->res_width = (int)desc.Width;
    out->res_height = (int)desc.Height;
    out->res_flags = (unsigned)desc.Flags;
    return 0;
}

// 0: a frame, 1: end of stream, <0: AVERROR.
// `want_texture` 0 skips the GPU copy of the D3D11VA way (the CPU path does not need the texture).
int spike_next(Spike *s, int want_texture, SpikeFrame *out) {
    AVFrame *f = av_frame_alloc();
    if (!f)
        return AVERROR(ENOMEM);
    int r = receive(s, f);
    if (r == 0 && f->format != (s->d3d11 ? AV_PIX_FMT_D3D11 : AV_PIX_FMT_D3D12))
        r = AVERROR(ENOSYS);
    if (r == 0) {
        enum AVPixelFormat sw = ((AVHWFramesContext *)f->hw_frames_ctx->data)->sw_format;
        if (sw != AV_PIX_FMT_NV12 && sw != AV_PIX_FMT_P010)
            r = AVERROR(ENOSYS);
        out->ten_bit = sw == AV_PIX_FMT_P010;
    }
    if (r == 0) {
        out->frame = f;
        out->width = f->width;
        out->height = f->height;
        out->pts = f->pts;
        if (s->d3d11) {
            if (want_texture)
                r = bridge_copy(s, f, out->ten_bit, out);
        } else {
            AVD3D12VAFrame *d = (AVD3D12VAFrame *)f->data[0];
            D3D12_RESOURCE_DESC desc;
            ID3D12Resource_GetDesc(d->texture, &desc);
            out->texture = d->texture;
            out->fence = d->sync_ctx.fence;
            out->fence_value = d->sync_ctx.fence_value;
            out->subresource = d->subresource_index;
            out->array_size = desc.DepthOrArraySize;
            out->res_width = (int)desc.Width;
            out->res_height = (int)desc.Height;
            out->res_flags = (unsigned)desc.Flags;
        }
    }
    if (r != 0)
        av_frame_free(&f);
    return r;
}

// The CPU detour: copies the frame into system memory (hwdownload). The planes stay valid until the next call.
int spike_download(Spike *s, void *frame, SpikePlanes *out) {
    av_frame_unref(s->sw);
    int r = av_hwframe_transfer_data(s->sw, frame, 0);
    if (r < 0)
        return r;
    for (int i = 0; i < 2; i++) {
        out->data[i] = s->sw->data[i];
        out->linesize[i] = s->sw->linesize[i];
    }
    return 0;
}

void spike_frame_free(void *frame) {
    AVFrame *f = frame;
    av_frame_free(&f);
}
