/*
 * OBS AirPlay Plugin
 * Main plugin entry point and source implementations
 */

#include <obs-module.h>
#include <obs-frontend-api.h>
#include <util/platform.h>
#include <util/threading.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// Rust FFI declarations
extern int obs_airplay_server_init(void);
extern int obs_airplay_server_start(unsigned short http_port, unsigned short rtsp_port);
extern void obs_airplay_server_stop(void);
extern int obs_airplay_server_is_running(void);
extern void obs_airplay_server_cleanup(void);
extern const char *obs_airplay_get_last_error(void);
extern void obs_airplay_free_string(char *s);

// Plugin info
OBS_DECLARE_MODULE()
OBS_MODULE_USE_DEFAULT_LOCALE("obs-airplay", "en-US")

// ============================================================================
// AirPlay Audio Source
// ============================================================================

struct airplay_audio_data {
    obs_source_t *source;
    bool active;
    pthread_mutex_t mutex;
};

static const char *airplay_audio_get_name(void *unused)
{
    UNUSED_PARAMETER(unused);
    return obs_module_text("AirplayAudio");
}

static void *airplay_audio_create(obs_data_t *settings, obs_source_t *source)
{
    struct airplay_audio_data *data = bzalloc(sizeof(struct airplay_audio_data));
    data->source = source;
    data->active = false;
    pthread_mutex_init(&data->mutex, NULL);

    // Ensure server is running
    if (!obs_airplay_server_is_running()) {
        obs_airplay_server_init();
        obs_airplay_server_start(7000, 5000);
    }

    return data;
}

static void airplay_audio_destroy(void *data)
{
    struct airplay_audio_data *context = data;
    if (context) {
        pthread_mutex_destroy(&context->mutex);
        bfree(context);
    }
}

static void airplay_audio_update(void *data, obs_data_t *settings)
{
    struct airplay_audio_data *context = data;
    pthread_mutex_lock(&context->mutex);
    // Update settings if needed
    pthread_mutex_unlock(&context->mutex);
}

static void airplay_audio_activate(void *data)
{
    struct airplay_audio_data *context = data;
    pthread_mutex_lock(&context->mutex);
    context->active = true;
    pthread_mutex_unlock(&context->mutex);
}

static void airplay_audio_deactivate(void *data)
{
    struct airplay_audio_data *context = data;
    pthread_mutex_lock(&context->mutex);
    context->active = false;
    pthread_mutex_unlock(&context->mutex);
}

static obs_properties_t *airplay_audio_properties(void *data)
{
    obs_properties_t *props = obs_properties_create();
    obs_properties_add_text(props, "info", obs_module_text("AirplayAudio.Info"),
                            OBS_TEXT_INFO);
    return props;
}

static void airplay_audio_tick(void *data, float seconds)
{
    struct airplay_audio_data *context = data;
    if (!context->active)
        return;

    // Process audio frames here
    // This will be connected to the Rust audio pipeline
}

struct obs_source_info airplay_audio_source_info = {
    .id = "airplay_audio",
    .type = OBS_SOURCE_TYPE_INPUT,
    .output_flags = OBS_SOURCE_AUDIO,
    .get_name = airplay_audio_get_name,
    .create = airplay_audio_create,
    .destroy = airplay_audio_destroy,
    .update = airplay_audio_update,
    .activate = airplay_audio_activate,
    .deactivate = airplay_audio_deactivate,
    .get_properties = airplay_audio_properties,
};

// ============================================================================
// AirPlay Video Source
// ============================================================================

struct airplay_video_data {
    obs_source_t *source;
    bool active;
    pthread_mutex_t mutex;
    uint32_t width;
    uint32_t height;
};

static const char *airplay_video_get_name(void *unused)
{
    UNUSED_PARAMETER(unused);
    return obs_module_text("AirplayVideo");
}

static void *airplay_video_create(obs_data_t *settings, obs_source_t *source)
{
    struct airplay_video_data *data = bzalloc(sizeof(struct airplay_video_data));
    data->source = source;
    data->active = false;
    data->width = 1920;
    data->height = 1080;
    pthread_mutex_init(&data->mutex, NULL);

    // Ensure server is running
    if (!obs_airplay_server_is_running()) {
        obs_airplay_server_init();
        obs_airplay_server_start(7000, 5000);
    }

    return data;
}

static void airplay_video_destroy(void *data)
{
    struct airplay_video_data *context = data;
    if (context) {
        pthread_mutex_destroy(&context->mutex);
        bfree(context);
    }
}

static void airplay_video_update(void *data, obs_data_t *settings)
{
    struct airplay_video_data *context = data;
    pthread_mutex_lock(&context->mutex);
    // Update settings if needed
    pthread_mutex_unlock(&context->mutex);
}

static void airplay_video_activate(void *data)
{
    struct airplay_video_data *context = data;
    pthread_mutex_lock(&context->mutex);
    context->active = true;
    pthread_mutex_unlock(&context->mutex);
}

static void airplay_video_deactivate(void *data)
{
    struct airplay_video_data *context = data;
    pthread_mutex_lock(&context->mutex);
    context->active = false;
    pthread_mutex_unlock(&context->mutex);
}

static obs_properties_t *airplay_video_properties(void *data)
{
    obs_properties_t *props = obs_properties_create();
    obs_properties_add_text(props, "info", obs_module_text("AirplayVideo.Info"),
                            OBS_TEXT_INFO);
    return props;
}

static void airplay_video_tick(void *data, float seconds)
{
    struct airplay_video_data *context = data;
    if (!context->active)
        return;

    // Process video frames here
    // This will be connected to the Rust video pipeline
}

static void airplay_video_render(void *data, gs_effect_t *effect)
{
    struct airplay_video_data *context = data;
    pthread_mutex_lock(&context->mutex);

    // Render video frame here
    // This will display the decoded AirPlay video

    pthread_mutex_unlock(&context->mutex);
    UNUSED_PARAMETER(effect);
}

static uint32_t airplay_video_get_width(void *data)
{
    struct airplay_video_data *context = data;
    pthread_mutex_lock(&context->mutex);
    uint32_t width = context->width;
    pthread_mutex_unlock(&context->mutex);
    return width;
}

static uint32_t airplay_video_get_height(void *data)
{
    struct airplay_video_data *context = data;
    pthread_mutex_lock(&context->mutex);
    uint32_t height = context->height;
    pthread_mutex_unlock(&context->mutex);
    return height;
}

struct obs_source_info airplay_video_source_info = {
    .id = "airplay_video",
    .type = OBS_SOURCE_TYPE_INPUT,
    .output_flags = OBS_SOURCE_VIDEO,
    .get_name = airplay_video_get_name,
    .create = airplay_video_create,
    .destroy = airplay_video_destroy,
    .update = airplay_video_update,
    .activate = airplay_video_activate,
    .deactivate = airplay_video_deactivate,
    .get_properties = airplay_video_properties,
    .video_tick = airplay_video_tick,
    .video_render = airplay_video_render,
    .get_width = airplay_video_get_width,
    .get_height = airplay_video_get_height,
};

// ============================================================================
// Module Entry Points
// ============================================================================

bool obs_module_load(void)
{
    // Initialize Rust library
    obs_airplay_server_init();

    // Register sources
    obs_register_source(&airplay_audio_source_info);
    obs_register_source(&airplay_video_source_info);

    blog(LOG_INFO, "OBS AirPlay Plugin loaded");
    return true;
}

void obs_module_unload(void)
{
    // Cleanup Rust library
    obs_airplay_server_cleanup();

    blog(LOG_INFO, "OBS AirPlay Plugin unloaded");
}

const char *obs_module_name(void)
{
    return "obs-airplay";
}

const char *obs_module_description(void)
{
    return obs_module_text("PluginDescription");
}
