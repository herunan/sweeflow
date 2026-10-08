#import "CPrivate.h"
#import <dlfcn.h>

#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
bool MFSetFrontWindow(pid_t pid, uint32_t wid) {
    static CGError (*setFront)(ProcessSerialNumber *, uint32_t, uint32_t);
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        void *sl = dlopen("/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight", RTLD_LAZY);
        if (sl) setFront = dlsym(sl, "_SLPSSetFrontProcessWithOptions");
    });
    ProcessSerialNumber psn = {0, 0};
    if (!setFront || GetProcessForPID(pid, &psn) != noErr) return false;
    // 0x200: kCPSUserGenerated, as if the user clicked the window.
    return setFront(&psn, wid, 0x200) == kCGErrorSuccess;
}
#pragma clang diagnostic pop
