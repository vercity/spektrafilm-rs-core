#include <exiv2/exiv2.hpp>

#include <memory>
#include <mutex>

namespace {

// Exiv2 can also parse the image's XMP while reading its EXIF. Keep the
// library's XMP initialization and all bridge operations serialized.
std::mutex metadata_mutex;

bool is_maker_note(const Exiv2::Exifdatum& datum) {
    const auto group = datum.groupName();
    return Exiv2::ExifTags::isMakerGroup(group) || group == "MakerNote" ||
           (group == "Photo" && datum.tag() == 0x927c);
}

}  // namespace

extern "C" void* spektrafilm_read_maker_note(const char* path) noexcept {
    try {
        std::lock_guard lock(metadata_mutex);
        auto source = Exiv2::ImageFactory::open(path);
        if (!source) {
            return nullptr;
        }
        source->readMetadata();
        auto note = std::make_unique<Exiv2::ExifData>();
        for (const auto& field : source->exifData()) {
            if (is_maker_note(field)) {
                note->add(field);
            }
        }
        if (note->empty()) {
            return nullptr;
        }
        // The camera identification helps Exiv2 choose the correct MakerNote writer.
        for (const char* key : {"Exif.Image.Make", "Exif.Image.Model"}) {
            const auto field = source->exifData().findKey(Exiv2::ExifKey(key));
            if (field != source->exifData().end()) {
                note->add(*field);
            }
        }
        return note.release();
    } catch (...) {
        return nullptr;
    }
}

extern "C" int spektrafilm_write_maker_note(const char* path, const void* note) noexcept {
    try {
        std::lock_guard lock(metadata_mutex);
        auto destination = Exiv2::ImageFactory::open(path);
        if (!destination) {
            return 1;
        }
        destination->readMetadata();
        auto& exif = destination->exifData();
        for (const auto& field : *static_cast<const Exiv2::ExifData*>(note)) {
            if (!is_maker_note(field)) {
                const auto existing = exif.findKey(Exiv2::ExifKey(field.key()));
                if (existing != exif.end()) {
                    exif.erase(existing);
                }
            }
            exif.add(field);
        }
        destination->writeXmpFromPacket(true);
        destination->writeMetadata();
        return 0;
    } catch (...) {
        return 1;
    }
}

extern "C" void spektrafilm_free_maker_note(void* note) noexcept {
    delete static_cast<Exiv2::ExifData*>(note);
}
