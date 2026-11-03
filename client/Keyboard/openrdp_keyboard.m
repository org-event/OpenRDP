// Loaded into sdl-freerdp with DYLD_INSERT_LIBRARIES by OpenRDP Client.
//
// SDL's macOS keyboard grab should stop the client Mac from taking system
// shortcuts (Cmd+Space, Cmd+Tab and so on) while the session window has focus,
// but that code is compiled only with -DSDL_MAC_NO_SANDBOX and Homebrew's SDL3
// is built without it. FreeRDP grabs the keyboard through
// SDL_SetWindowKeyboardGrab when the pointer enters its window and releases it
// when the pointer leaves, so this library interposes that call and does what
// SDL would have done, but only while the client is also the active app (the
// pointer can rest over the window while another app has focus). It also clears
// the Cmd shortcuts of SDL's default menu (Cmd+Q, Cmd+W, Cmd+H, Cmd+M), so those
// keys reach the remote Mac instead of quitting or hiding the client.
//
// OPENRDP_KEYBOARD_DEBUG=1 logs every grab change to stderr (the session log).

#import <AppKit/AppKit.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>

typedef int CGSConnection;
enum { CGSGlobalHotKeyEnable = 0, CGSGlobalHotKeyDisable = 1 };
extern CGSConnection _CGSDefaultConnection(void);
extern CGError CGSSetGlobalHotKeyOperatingMode(CGSConnection connection, int mode);

// Resolved from libSDL3 at load time (linked with -undefined dynamic_lookup).
// Weak, so a client without SDL3 still starts; there is nothing to interpose then.
extern bool SDL_SetWindowKeyboardGrab(void *window, bool grabbed) __attribute__((weak_import));

static bool hotkeys_disabled = false;
static bool grab_requested = false;

static void set_system_hotkeys(bool disabled)
{
    if (disabled == hotkeys_disabled)
        return;
    CGError err = CGSSetGlobalHotKeyOperatingMode(_CGSDefaultConnection(),
                                                  disabled ? CGSGlobalHotKeyDisable : CGSGlobalHotKeyEnable);
    hotkeys_disabled = disabled;
    fprintf(stderr, "[openrdp-keyboard] %.3f system shortcuts %s (%d)\n", [NSDate date].timeIntervalSince1970,
            disabled ? "passed to the session" : "back to this Mac", err);
}

static void strip_command_shortcuts(NSMenu *menu)
{
    for (NSMenuItem *item in menu.itemArray) {
        if (item.keyEquivalent.length > 0 && (item.keyEquivalentModifierMask & NSEventModifierFlagCommand))
            item.keyEquivalent = @"";
        if (item.submenu)
            strip_command_shortcuts(item.submenu);
    }
}

static void update_system_hotkeys(void)
{
    if (getenv("OPENRDP_KEYBOARD_DEBUG"))
        fprintf(stderr, "[openrdp-keyboard] %.3f grab=%d active=%d\n", [NSDate date].timeIntervalSince1970, grab_requested, NSApp.isActive);
    set_system_hotkeys(grab_requested && NSApp.isActive);
}

static bool openrdp_SetWindowKeyboardGrab(void *window, bool grabbed)
{
    bool ok = SDL_SetWindowKeyboardGrab(window, grabbed);
    grab_requested = grabbed;
    update_system_hotkeys();
    if (NSApp.mainMenu)
        strip_command_shortcuts(NSApp.mainMenu);
    return ok;
}

__attribute__((used)) static const struct {
    const void *replacement;
    const void *replacee;
} interposers[] __attribute__((section("__DATA,__interpose"))) = {
    { (const void *)openrdp_SetWindowKeyboardGrab, (const void *)SDL_SetWindowKeyboardGrab },
};

__attribute__((constructor)) static void openrdp_keyboard_init(void)
{
    for (NSNotificationName name in @[ NSApplicationDidBecomeActiveNotification, NSApplicationDidResignActiveNotification ]) {
        [[NSNotificationCenter defaultCenter] addObserverForName:name
                                                          object:nil
                                                           queue:nil
                                                      usingBlock:^(NSNotification *note) {
                                                          (void)note;
                                                          update_system_hotkeys();
                                                      }];
    }
}
