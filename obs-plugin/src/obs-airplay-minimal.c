#include <obs-module.h>

OBS_DECLARE_MODULE()
OBS_MODULE_USE_DEFAULT_LOCALE("en-US", "en-US")

bool obs_module_load(void)
{
    blog(LOG_INFO, "OBS AirPlay Plugin loaded (minimal version)");
    blog(LOG_INFO, "Note: Use the standalone receiver for full functionality");
    blog(LOG_INFO, "Run: /Users/eklavya/Desktop/ux-play-rust/release/rusty-play");
    return true;
}

void obs_module_unload(void)
{
    blog(LOG_INFO, "OBS AirPlay Plugin unloaded");
}

const char *obs_module_name(void)
{
    return "obs-airplay";
}

const char *obs_module_description(void)
{
    return "AirPlay Receiver for OBS (use standalone receiver for full functionality)";
}
