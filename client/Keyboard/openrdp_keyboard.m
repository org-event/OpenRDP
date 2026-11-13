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
// With OPENRDP_LOCAL_CONTROL_SHORTCUTS=1 the system shortcuts come back to this
// Mac while ⌃ is the only modifier held, so ⌃← and ⌃→ switch its Spaces and ⌃↑
// opens its Mission Control, also from a session in full screen. WindowServer
// matches a shortcut when its last key goes down, so turning them on at ⌃ is in
// time. Disabling single shortcuts (CGSSetSymbolicHotKeyEnabled) instead would
// outlive a crashed client, while the operating mode belongs to this connection.
//
// OPENRDP_KEYBOARD_DEBUG=1 logs every grab change to stderr (the session log).

#import <AppKit/AppKit.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef int CGSConnection;
enum { CGSGlobalHotKeyEnable = 0, CGSGlobalHotKeyDisable = 1 };
extern CGSConnection _CGSDefaultConnection(void);
extern CGError CGSSetGlobalHotKeyOperatingMode(CGSConnection connection, int mode);

// Resolved from libSDL3 at load time (linked with -undefined dynamic_lookup).
// Weak, so a client without SDL3 still starts; there is nothing to interpose then.
extern bool SDL_SetWindowKeyboardGrab(void *window, bool grabbed) __attribute__((weak_import));

static bool hotkeys_disabled = false;
static bool grab_requested = false;
static bool local_control_shortcuts = false;
static bool control_only = false;

// Always applied: an SDL built with SDL_MAC_NO_SANDBOX sets the same mode itself
// on every grab and focus change, so what we last set says nothing about it now.
static void set_system_hotkeys(bool disabled)
{
    CGError err = CGSSetGlobalHotKeyOperatingMode(_CGSDefaultConnection(),
                                                  disabled ? CGSGlobalHotKeyDisable : CGSGlobalHotKeyEnable);
    if (disabled != hotkeys_disabled || err)
        fprintf(stderr, "[openrdp-keyboard] %.3f system shortcuts %s (%d)\n", [NSDate date].timeIntervalSince1970,
                disabled ? "passed to the session" : "back to this Mac", err);
    hotkeys_disabled = disabled;
}

static bool is_control_only(NSEventModifierFlags flags)
{
    const NSEventModifierFlags mods = NSEventModifierFlagControl | NSEventModifierFlagOption |
                                      NSEventModifierFlagCommand | NSEventModifierFlagShift;
    return (flags & mods) == NSEventModifierFlagControl;
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
        fprintf(stderr, "[openrdp-keyboard] %.3f grab=%d active=%d control=%d\n", [NSDate date].timeIntervalSince1970,
                grab_requested, NSApp.isActive, control_only);
    set_system_hotkeys(grab_requested && NSApp.isActive && !(local_control_shortcuts && control_only));
}

// After SDL has handled the same change (focus, key window) and set the mode.
static void update_system_hotkeys_soon(void)
{
    dispatch_async(dispatch_get_main_queue(), ^{
        update_system_hotkeys();
    });
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
    const char *local = getenv("OPENRDP_LOCAL_CONTROL_SHORTCUTS");
    local_control_shortcuts = local && strcmp(local, "1") == 0;

    for (NSNotificationName name in @[ NSApplicationDidBecomeActiveNotification, NSApplicationDidResignActiveNotification,
                                       NSWindowDidBecomeKeyNotification, NSWindowDidResignKeyNotification ]) {
        [[NSNotificationCenter defaultCenter] addObserverForName:name
                                                          object:nil
                                                           queue:nil
                                                      usingBlock:^(NSNotification *note) {
                                                          (void)note;
                                                          // ⌃ may have gone up while another Space was shown.
                                                          control_only = is_control_only(NSEvent.modifierFlags);
                                                          update_system_hotkeys();
                                                          update_system_hotkeys_soon();
                                                      }];
    }

    if (!local_control_shortcuts)
        return;
    // NSApp does not exist yet while the library loads.
    dispatch_async(dispatch_get_main_queue(), ^{
        [NSEvent addLocalMonitorForEventsMatchingMask:NSEventMaskFlagsChanged
                                              handler:^NSEvent *(NSEvent *event) {
                                                  bool now = is_control_only(event.modifierFlags);
                                                  if (now != control_only) {
                                                      control_only = now;
                                                      update_system_hotkeys();
                                                  }
                                                  return event;
                                              }];
    });
}
