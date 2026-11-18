// OpenRDP Microphone: a Core Audio server plug-in (AudioServerPlugIn).
//
// Two devices share one ring buffer and one clock:
//
// - "OpenRDP Microphone" (input, visible): what apps on this Mac record from.
// - "OpenRDP Microphone Feed" (output, hidden): the OpenRDP server plays the
//   client's microphone (MS-RDPEAI, AUDIO_INPUT) into it.
//
// The feed writes each frame at its sample time; the microphone reads the
// same sample time a little later and clears it, so it hears silence when
// nothing feeds it. Both devices count samples from the same host-time anchor,
// so a sample time means the same moment on both. No IPC with the server
// process: Core Audio carries the samples from the server to this driver.
//
// 48 kHz mono 32-bit float only; apps and the server convert.

#include <CoreAudio/AudioServerPlugIn.h>
#include <mach/mach_time.h>
#include <math.h>
#include <pthread.h>
#include <string.h>

enum {
    kPlugIn = kAudioObjectPlugInObject,
    kMic = 2,
    kMicStream = 3,
    kFeed = 4,
    kFeedStream = 5,
};

#define RATE 48000.0
// Frames in the ring, and the zero time stamp period. 340 ms at 48 kHz:
// far more than the distance between the feed's write and the mic's read.
#define RING 16384u

#define MIC_UID "OpenRDPMicrophone_UID"
#define FEED_UID "OpenRDPMicrophoneFeed_UID"

static volatile Float32 gRing[RING];
static AudioServerPlugInHostRef gHost;
static UInt64 gAnchor;
static Float64 gTicksPerFrame;
static pthread_mutex_t gLock = PTHREAD_MUTEX_INITIALIZER;
// IO clients per device: [0] microphone, [1] feed.
static UInt32 gRunning[2];
static UInt32 gStreamActive[2] = {1, 1};
static UInt32 gRefCount;

static int device_index(AudioObjectID id) {
    return id == kMic ? 0 : id == kFeed ? 1 : -1;
}

static int stream_index(AudioObjectID id) {
    return id == kMicStream ? 0 : id == kFeedStream ? 1 : -1;
}

static AudioStreamBasicDescription format(void) {
    AudioStreamBasicDescription f = {0};
    f.mSampleRate = RATE;
    f.mFormatID = kAudioFormatLinearPCM;
    f.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagsNativeEndian | kAudioFormatFlagIsPacked;
    f.mBytesPerPacket = 4;
    f.mFramesPerPacket = 1;
    f.mBytesPerFrame = 4;
    f.mChannelsPerFrame = 1;
    f.mBitsPerChannel = 32;
    return f;
}

// ---------------------------------------------------------------------------
// Properties. One function answers HasProperty, GetPropertyDataSize and
// GetPropertyData: with out == NULL it only reports the size.

#define SCALAR(T, value)                                                       \
    do {                                                                       \
        *size = sizeof(T);                                                     \
        if (out) {                                                             \
            if (cap < sizeof(T)) return kAudioHardwareBadPropertySizeError;    \
            *(T *)out = (value);                                               \
        }                                                                      \
        return kAudioHardwareNoError;                                          \
    } while (0)

#define ARRAY(T, items, count)                                                 \
    do {                                                                       \
        UInt32 n_ = (count);                                                   \
        if (out) {                                                             \
            if (n_ > cap / sizeof(T)) n_ = cap / sizeof(T);                    \
            if (n_) memcpy(out, (items), n_ * sizeof(T));                      \
        }                                                                      \
        *size = n_ * (UInt32)sizeof(T);                                        \
        return kAudioHardwareNoError;                                          \
    } while (0)

static OSStatus plugin_property(const AudioObjectPropertyAddress *a, UInt32 qsize,
                                const void *q, UInt32 cap, UInt32 *size, void *out) {
    static const AudioObjectID devices[] = {kMic, kFeed};
    switch (a->mSelector) {
    case kAudioObjectPropertyBaseClass: SCALAR(AudioClassID, kAudioObjectClassID);
    case kAudioObjectPropertyClass: SCALAR(AudioClassID, kAudioPlugInClassID);
    case kAudioObjectPropertyOwner: SCALAR(AudioObjectID, kAudioObjectUnknown);
    case kAudioObjectPropertyManufacturer: SCALAR(CFStringRef, CFSTR("OpenRDP"));
    case kAudioObjectPropertyOwnedObjects:
    case kAudioPlugInPropertyDeviceList: ARRAY(AudioObjectID, devices, 2);
    case kAudioPlugInPropertyBoxList: ARRAY(AudioObjectID, devices, 0);
    case kAudioPlugInPropertyTranslateUIDToBox: SCALAR(AudioObjectID, kAudioObjectUnknown);
    case kAudioPlugInPropertyTranslateUIDToDevice: {
        AudioObjectID found = kAudioObjectUnknown;
        if (out && qsize == sizeof(CFStringRef) && q) {
            CFStringRef uid = *(const CFStringRef *)q;
            if (uid && CFEqual(uid, CFSTR(MIC_UID))) found = kMic;
            else if (uid && CFEqual(uid, CFSTR(FEED_UID))) found = kFeed;
        }
        SCALAR(AudioObjectID, found);
    }
    case kAudioPlugInPropertyResourceBundle: SCALAR(CFStringRef, CFSTR(""));
    default: return kAudioHardwareUnknownPropertyError;
    }
}

static OSStatus device_property(AudioObjectID id, const AudioObjectPropertyAddress *a,
                                UInt32 qsize, const void *q, UInt32 cap, UInt32 *size,
                                void *out) {
    int mic = id == kMic;
    int i = device_index(id);
    AudioObjectID stream = mic ? kMicStream : kFeedStream;
    AudioObjectPropertyScope own = mic ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput;
    switch (a->mSelector) {
    case kAudioObjectPropertyBaseClass: SCALAR(AudioClassID, kAudioObjectClassID);
    case kAudioObjectPropertyClass: SCALAR(AudioClassID, kAudioDeviceClassID);
    case kAudioObjectPropertyOwner: SCALAR(AudioObjectID, kPlugIn);
    case kAudioObjectPropertyName:
        SCALAR(CFStringRef, mic ? CFSTR("OpenRDP Microphone") : CFSTR("OpenRDP Microphone Feed"));
    case kAudioObjectPropertyManufacturer: SCALAR(CFStringRef, CFSTR("OpenRDP"));
    case kAudioObjectPropertyOwnedObjects: {
        UInt32 n = 1;
        if (qsize >= sizeof(AudioClassID) && q) {
            AudioClassID want = *(const AudioClassID *)q;
            n = want == kAudioStreamClassID || want == kAudioObjectClassID;
        }
        ARRAY(AudioObjectID, &stream, n);
    }
    case kAudioDevicePropertyDeviceUID: SCALAR(CFStringRef, mic ? CFSTR(MIC_UID) : CFSTR(FEED_UID));
    case kAudioDevicePropertyModelUID: SCALAR(CFStringRef, CFSTR("OpenRDPMicrophone_ModelUID"));
    case kAudioDevicePropertyTransportType: SCALAR(UInt32, kAudioDeviceTransportTypeVirtual);
    case kAudioDevicePropertyRelatedDevices: ARRAY(AudioObjectID, &id, 1);
    case kAudioDevicePropertyClockDomain: SCALAR(UInt32, 0);
    case kAudioDevicePropertyDeviceIsAlive: SCALAR(UInt32, 1);
    case kAudioDevicePropertyDeviceIsRunning: {
        pthread_mutex_lock(&gLock);
        UInt32 running = gRunning[i] > 0;
        pthread_mutex_unlock(&gLock);
        SCALAR(UInt32, running);
    }
    case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        SCALAR(UInt32, mic && a->mScope == kAudioObjectPropertyScopeInput);
    case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice: SCALAR(UInt32, 0);
    case kAudioDevicePropertyLatency: SCALAR(UInt32, 0);
    case kAudioDevicePropertySafetyOffset: SCALAR(UInt32, 0);
    case kAudioDevicePropertyStreams: {
        UInt32 n = a->mScope == kAudioObjectPropertyScopeGlobal || a->mScope == own;
        ARRAY(AudioObjectID, &stream, n);
    }
    case kAudioObjectPropertyControlList: ARRAY(AudioObjectID, &stream, 0);
    case kAudioDevicePropertyNominalSampleRate: SCALAR(Float64, RATE);
    case kAudioDevicePropertyAvailableNominalSampleRates: {
        AudioValueRange range = {RATE, RATE};
        ARRAY(AudioValueRange, &range, 1);
    }
    case kAudioDevicePropertyIsHidden: SCALAR(UInt32, !mic);
    case kAudioDevicePropertyZeroTimeStampPeriod: SCALAR(UInt32, RING);
    case kAudioDevicePropertyPreferredChannelsForStereo: {
        UInt32 channels[2] = {1, 1};
        ARRAY(UInt32, channels, 2);
    }
    default: return kAudioHardwareUnknownPropertyError;
    }
}

static OSStatus stream_property(AudioObjectID id, const AudioObjectPropertyAddress *a,
                                UInt32 cap, UInt32 *size, void *out) {
    int mic = id == kMicStream;
    int i = stream_index(id);
    switch (a->mSelector) {
    case kAudioObjectPropertyBaseClass: SCALAR(AudioClassID, kAudioObjectClassID);
    case kAudioObjectPropertyClass: SCALAR(AudioClassID, kAudioStreamClassID);
    case kAudioObjectPropertyOwner: SCALAR(AudioObjectID, mic ? kMic : kFeed);
    case kAudioStreamPropertyIsActive: {
        pthread_mutex_lock(&gLock);
        UInt32 active = gStreamActive[i];
        pthread_mutex_unlock(&gLock);
        SCALAR(UInt32, active);
    }
    case kAudioStreamPropertyDirection: SCALAR(UInt32, mic ? 1 : 0);
    case kAudioStreamPropertyTerminalType:
        SCALAR(UInt32, mic ? kAudioStreamTerminalTypeMicrophone : kAudioStreamTerminalTypeLine);
    case kAudioStreamPropertyStartingChannel: SCALAR(UInt32, 1);
    case kAudioStreamPropertyLatency: SCALAR(UInt32, 0);
    case kAudioStreamPropertyVirtualFormat:
    case kAudioStreamPropertyPhysicalFormat: SCALAR(AudioStreamBasicDescription, format());
    case kAudioStreamPropertyAvailableVirtualFormats:
    case kAudioStreamPropertyAvailablePhysicalFormats: {
        AudioStreamRangedDescription ranged = {format(), {RATE, RATE}};
        ARRAY(AudioStreamRangedDescription, &ranged, 1);
    }
    default: return kAudioHardwareUnknownPropertyError;
    }
}

static OSStatus property(AudioObjectID id, const AudioObjectPropertyAddress *a, UInt32 qsize,
                         const void *q, UInt32 cap, UInt32 *size, void *out) {
    if (!a || !size) return kAudioHardwareIllegalOperationError;
    if (id == kPlugIn) return plugin_property(a, qsize, q, cap, size, out);
    if (device_index(id) >= 0) return device_property(id, a, qsize, q, cap, size, out);
    if (stream_index(id) >= 0) return stream_property(id, a, cap, size, out);
    return kAudioHardwareBadObjectError;
}

// ---------------------------------------------------------------------------
// The driver interface.

static HRESULT QueryInterface(void *driver, REFIID uuid, LPVOID *out);
static ULONG AddRef(void *driver);
static ULONG Release(void *driver);

static OSStatus Initialize(AudioServerPlugInDriverRef driver, AudioServerPlugInHostRef host) {
    (void)driver;
    gHost = host;
    mach_timebase_info_data_t base;
    mach_timebase_info(&base);
    gTicksPerFrame = (1e9 * (Float64)base.denom / (Float64)base.numer) / RATE;
    gAnchor = mach_absolute_time();
    return kAudioHardwareNoError;
}

static OSStatus CreateDevice(AudioServerPlugInDriverRef d, CFDictionaryRef desc,
                             const AudioServerPlugInClientInfo *client, AudioObjectID *out) {
    (void)d; (void)desc; (void)client; (void)out;
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus DestroyDevice(AudioServerPlugInDriverRef d, AudioObjectID id) {
    (void)d; (void)id;
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus AddDeviceClient(AudioServerPlugInDriverRef d, AudioObjectID id,
                                const AudioServerPlugInClientInfo *client) {
    (void)d; (void)id; (void)client;
    return kAudioHardwareNoError;
}

static OSStatus RemoveDeviceClient(AudioServerPlugInDriverRef d, AudioObjectID id,
                                   const AudioServerPlugInClientInfo *client) {
    (void)d; (void)id; (void)client;
    return kAudioHardwareNoError;
}

static OSStatus PerformDeviceConfigurationChange(AudioServerPlugInDriverRef d, AudioObjectID id,
                                                 UInt64 action, void *info) {
    (void)d; (void)id; (void)action; (void)info;
    return kAudioHardwareNoError;
}

static OSStatus AbortDeviceConfigurationChange(AudioServerPlugInDriverRef d, AudioObjectID id,
                                               UInt64 action, void *info) {
    (void)d; (void)id; (void)action; (void)info;
    return kAudioHardwareNoError;
}

static Boolean HasProperty(AudioServerPlugInDriverRef d, AudioObjectID id, pid_t pid,
                           const AudioObjectPropertyAddress *a) {
    (void)d; (void)pid;
    UInt32 size = 0;
    return property(id, a, 0, NULL, 0, &size, NULL) == kAudioHardwareNoError;
}

static OSStatus IsPropertySettable(AudioServerPlugInDriverRef d, AudioObjectID id, pid_t pid,
                                   const AudioObjectPropertyAddress *a, Boolean *out) {
    (void)d; (void)pid;
    UInt32 size = 0;
    OSStatus status = property(id, a, 0, NULL, 0, &size, NULL);
    if (status != kAudioHardwareNoError) return status;
    if (!out) return kAudioHardwareIllegalOperationError;
    *out = stream_index(id) >= 0 && a->mSelector == kAudioStreamPropertyIsActive;
    return kAudioHardwareNoError;
}

static OSStatus GetPropertyDataSize(AudioServerPlugInDriverRef d, AudioObjectID id, pid_t pid,
                                    const AudioObjectPropertyAddress *a, UInt32 qsize,
                                    const void *q, UInt32 *size) {
    (void)d; (void)pid;
    return property(id, a, qsize, q, 0, size, NULL);
}

static OSStatus GetPropertyData(AudioServerPlugInDriverRef d, AudioObjectID id, pid_t pid,
                                const AudioObjectPropertyAddress *a, UInt32 qsize, const void *q,
                                UInt32 cap, UInt32 *size, void *out) {
    (void)d; (void)pid;
    if (!out) return kAudioHardwareIllegalOperationError;
    return property(id, a, qsize, q, cap, size, out);
}

static OSStatus SetPropertyData(AudioServerPlugInDriverRef d, AudioObjectID id, pid_t pid,
                                const AudioObjectPropertyAddress *a, UInt32 qsize, const void *q,
                                UInt32 size, const void *data) {
    (void)d; (void)pid; (void)qsize; (void)q;
    int i = stream_index(id);
    if (i >= 0 && a && a->mSelector == kAudioStreamPropertyIsActive) {
        if (size != sizeof(UInt32) || !data) return kAudioHardwareBadPropertySizeError;
        pthread_mutex_lock(&gLock);
        gStreamActive[i] = *(const UInt32 *)data != 0;
        pthread_mutex_unlock(&gLock);
        return kAudioHardwareNoError;
    }
    return kAudioHardwareUnsupportedOperationError;
}

static void running_changed(AudioObjectID id) {
    if (!gHost) return;
    AudioObjectPropertyAddress a = {kAudioDevicePropertyDeviceIsRunning,
                                    kAudioObjectPropertyScopeGlobal,
                                    kAudioObjectPropertyElementMain};
    gHost->PropertiesChanged(gHost, id, 1, &a);
}

static OSStatus StartIO(AudioServerPlugInDriverRef d, AudioObjectID id, UInt32 client) {
    (void)d; (void)client;
    int i = device_index(id);
    if (i < 0) return kAudioHardwareBadObjectError;
    pthread_mutex_lock(&gLock);
    int first = gRunning[i]++ == 0;
    // Whatever the feed left while nobody listened is old by now.
    if (first && i == 0)
        for (UInt32 k = 0; k < RING; k++) gRing[k] = 0;
    pthread_mutex_unlock(&gLock);
    if (first) running_changed(id);
    return kAudioHardwareNoError;
}

static OSStatus StopIO(AudioServerPlugInDriverRef d, AudioObjectID id, UInt32 client) {
    (void)d; (void)client;
    int i = device_index(id);
    if (i < 0) return kAudioHardwareBadObjectError;
    pthread_mutex_lock(&gLock);
    int last = gRunning[i] > 0 && --gRunning[i] == 0;
    pthread_mutex_unlock(&gLock);
    if (last) running_changed(id);
    return kAudioHardwareNoError;
}

static OSStatus GetZeroTimeStamp(AudioServerPlugInDriverRef d, AudioObjectID id, UInt32 client,
                                 Float64 *sample_time, UInt64 *host_time, UInt64 *seed) {
    (void)d; (void)client;
    if (device_index(id) < 0) return kAudioHardwareBadObjectError;
    // Both devices count from the same anchor: one clock, one sample time.
    Float64 period = gTicksPerFrame * RING;
    Float64 periods = floor((Float64)(mach_absolute_time() - gAnchor) / period);
    *sample_time = periods * RING;
    *host_time = gAnchor + (UInt64)(periods * period);
    *seed = 1;
    return kAudioHardwareNoError;
}

static OSStatus WillDoIOOperation(AudioServerPlugInDriverRef d, AudioObjectID id, UInt32 client,
                                  UInt32 op, Boolean *will, Boolean *in_place) {
    (void)d; (void)client;
    Boolean yes = (id == kMic && op == kAudioServerPlugInIOOperationReadInput) ||
                  (id == kFeed && op == kAudioServerPlugInIOOperationWriteMix);
    if (will) *will = yes;
    if (in_place) *in_place = true;
    return kAudioHardwareNoError;
}

static OSStatus BeginIOOperation(AudioServerPlugInDriverRef d, AudioObjectID id, UInt32 client,
                                 UInt32 op, UInt32 frames, const AudioServerPlugInIOCycleInfo *info) {
    (void)d; (void)id; (void)client; (void)op; (void)frames; (void)info;
    return kAudioHardwareNoError;
}

static OSStatus DoIOOperation(AudioServerPlugInDriverRef d, AudioObjectID id, AudioObjectID stream,
                              UInt32 client, UInt32 op, UInt32 frames,
                              const AudioServerPlugInIOCycleInfo *info, void *main, void *secondary) {
    (void)d; (void)stream; (void)client; (void)secondary;
    if (!main || !info) return kAudioHardwareNoError;
    Float32 *buffer = main;
    if (id == kMic && op == kAudioServerPlugInIOOperationReadInput) {
        UInt64 start = (UInt64)info->mInputTime.mSampleTime;
        for (UInt32 k = 0; k < frames; k++) {
            UInt32 slot = (UInt32)((start + k) % RING);
            buffer[k] = gRing[slot];
            gRing[slot] = 0;
        }
    } else if (id == kFeed && op == kAudioServerPlugInIOOperationWriteMix) {
        UInt64 start = (UInt64)info->mOutputTime.mSampleTime;
        for (UInt32 k = 0; k < frames; k++) gRing[(start + k) % RING] = buffer[k];
    }
    return kAudioHardwareNoError;
}

static OSStatus EndIOOperation(AudioServerPlugInDriverRef d, AudioObjectID id, UInt32 client,
                               UInt32 op, UInt32 frames, const AudioServerPlugInIOCycleInfo *info) {
    (void)d; (void)id; (void)client; (void)op; (void)frames; (void)info;
    return kAudioHardwareNoError;
}

static AudioServerPlugInDriverInterface gInterface = {
    NULL,
    QueryInterface,
    AddRef,
    Release,
    Initialize,
    CreateDevice,
    DestroyDevice,
    AddDeviceClient,
    RemoveDeviceClient,
    PerformDeviceConfigurationChange,
    AbortDeviceConfigurationChange,
    HasProperty,
    IsPropertySettable,
    GetPropertyDataSize,
    GetPropertyData,
    SetPropertyData,
    StartIO,
    StopIO,
    GetZeroTimeStamp,
    WillDoIOOperation,
    BeginIOOperation,
    DoIOOperation,
    EndIOOperation,
};
static AudioServerPlugInDriverInterface *gInterfacePtr = &gInterface;
static AudioServerPlugInDriverRef gDriver = &gInterfacePtr;

static HRESULT QueryInterface(void *driver, REFIID uuid, LPVOID *out) {
    if (driver != gDriver || !out) return kAudioHardwareBadObjectError;
    CFUUIDRef asked = CFUUIDCreateFromUUIDBytes(NULL, uuid);
    if (!asked) return kAudioHardwareIllegalOperationError;
    Boolean ok = CFEqual(asked, IUnknownUUID) || CFEqual(asked, kAudioServerPlugInDriverInterfaceUUID);
    CFRelease(asked);
    if (!ok) return E_NOINTERFACE;
    pthread_mutex_lock(&gLock);
    gRefCount++;
    pthread_mutex_unlock(&gLock);
    *out = gDriver;
    return S_OK;
}

static ULONG AddRef(void *driver) {
    if (driver != gDriver) return 0;
    pthread_mutex_lock(&gLock);
    ULONG count = ++gRefCount;
    pthread_mutex_unlock(&gLock);
    return count;
}

static ULONG Release(void *driver) {
    if (driver != gDriver) return 0;
    pthread_mutex_lock(&gLock);
    ULONG count = gRefCount > 0 ? --gRefCount : 0;
    pthread_mutex_unlock(&gLock);
    return count;
}

// The factory named in Info.plist (CFPlugInFactories).
__attribute__((visibility("default"))) void *OpenRDPAudio_Create(CFAllocatorRef allocator,
                                                                 CFUUIDRef type) {
    (void)allocator;
    return CFEqual(type, kAudioServerPlugInTypeUUID) ? gDriver : NULL;
}
