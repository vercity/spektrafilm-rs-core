#include <cstddef>
#include <cstdint>
#include <limits>
#include <memory>
#include <new>

#include <libraw/libraw.h>

static_assert(LIBRAW_COMPILE_CHECK_VERSION_NOTLESS(0, 22), "LibRaw 0.22 or later is required");

struct DevelopedImage {
    void *handle;
    const std::uint8_t *samples;
    std::size_t length;
    std::uint32_t width;
    std::uint32_t height;
};

static int decode_status(int status) noexcept {
    if (status == LIBRAW_FILE_UNSUPPORTED || status == LIBRAW_NOT_IMPLEMENTED) {
        return 1; // Rust can still try its own RAW decoder.
    }
    return status;
}

extern "C" int spektrafilm_develop_raw(
    std::uint8_t *file, std::size_t length, int camera_wb, DevelopedImage *output
) noexcept {
    *output = {};
    // A header and library mismatch makes accesses to LibRaw's parameter
    // fields unsafe. Validate before opening a decoder or reading any fields.
    if ((libraw_versionNumber() & 0xffff00) != (LIBRAW_VERSION & 0xffff00)) {
        return -10001;
    }
    try {
        std::unique_ptr<libraw_data_t, decltype(&libraw_close)> decoder(
            libraw_init(0), libraw_close
        );
        if (!decoder) {
            return LIBRAW_UNSUFFICIENT_MEMORY;
        }
        auto status = libraw_open_buffer(decoder.get(), file, length);
        if (status != LIBRAW_SUCCESS) {
            return decode_status(status);
        }
        if ((status = libraw_unpack(decoder.get())) != LIBRAW_SUCCESS) {
            return decode_status(status);
        }
        auto &parameters = decoder->params;
        parameters.output_color = 6; // ACES2065-1
        parameters.output_bps = 16;
        parameters.no_auto_bright = 1;
        parameters.gamm[0] = parameters.gamm[1] = 1.0;
        parameters.use_camera_wb = camera_wb;

        if ((status = libraw_dcraw_process(decoder.get())) != LIBRAW_SUCCESS) {
            return decode_status(status);
        }
        std::unique_ptr<libraw_processed_image_t, decltype(&libraw_dcraw_clear_mem)> image(
            libraw_dcraw_make_mem_image(decoder.get(), &status), libraw_dcraw_clear_mem
        );
        if (status != LIBRAW_SUCCESS) {
            return decode_status(status);
        }
        if (!image || image->type != LIBRAW_IMAGE_BITMAP || image->bits != 16) {
            return -10002;
        }
        if (image->colors != 3) {
            return 1;
        }
        const std::uint64_t size = std::uint64_t(image->width) * image->height * 6;
        if (size == 0 || size != image->data_size ||
            size > std::uint64_t(std::numeric_limits<std::ptrdiff_t>::max())) {
            return -10002;
        }
        *output = {image.get(), image->data, image->data_size, image->width, image->height};
        image.release();
        return LIBRAW_SUCCESS;
    } catch (const std::bad_alloc &) {
        return LIBRAW_UNSUFFICIENT_MEMORY;
    } catch (...) {
        return LIBRAW_UNSPECIFIED_ERROR;
    }
}

extern "C" void spektrafilm_free_raw(void *image) noexcept {
    libraw_dcraw_clear_mem(static_cast<libraw_processed_image_t *>(image));
}
