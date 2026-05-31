cmake_minimum_required(VERSION 3.10)
project(obs-airplay VERSION 0.1.0)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_C_STANDARD 11)

# Find OBS Studio
if(NOT LIBOBS_INCLUDE_DIR)
    find_path(LIBOBS_INCLUDE_DIR
        NAMES obs.h
        PATHS
            "/tmp/obs-headers"
            "/tmp/obs-studio/libobs"
            "/Applications/OBS.app/Contents/Frameworks/libobs.framework/Headers"
            "${CMAKE_SOURCE_DIR}/../../libobs"
            "${CMAKE_SOURCE_DIR}/../obs-studio/libobs"
            "/usr/include/obs"
            "/usr/local/include/obs"
            "C:/Program Files/obs-studio/include"
            "C:/Program Files (x86)/obs-studio/include"
    )
endif()

if(NOT LIBOBS_LIB)
    find_library(LIBOBS_LIB
        NAMES obs
        PATHS
            "/Applications/OBS.app/Contents/Frameworks/libobs.framework"
            "${CMAKE_SOURCE_DIR}/../../build/libobs"
            "${CMAKE_SOURCE_DIR}/../obs-studio/build/libobs"
            "/usr/lib"
            "/usr/local/lib"
            "C:/Program Files/obs-studio/bin/64bit"
            "C:/Program Files (x86)/obs-studio/bin/64bit"
    )
endif()

if(NOT LIBOBS_INCLUDE_DIR OR NOT LIBOBS_LIB)
    message(FATAL_ERROR "Could not find OBS Studio. Please set LIBOBS_INCLUDE_DIR and LIBOBS_LIB")
endif()

message(STATUS "Found OBS Studio include: ${LIBOBS_INCLUDE_DIR}")
message(STATUS "Found OBS Studio library: ${LIBOBS_LIB}")

# Create plugin library
add_library(obs-airplay MODULE
    src/obs-airplay-minimal.c
)

target_include_directories(obs-airplay PRIVATE
    ${LIBOBS_INCLUDE_DIR}
    ${LIBOBS_INCLUDE_DIR}/util
    ${LIBOBS_INCLUDE_DIR}/graphics
    ${LIBOBS_INCLUDE_DIR}/media-io
    ${LIBOBS_INCLUDE_DIR}/callback
    /opt/homebrew/include
)

target_link_libraries(obs-airplay PRIVATE
    ${LIBOBS_LIB}
)

# Set output properties
set_target_properties(obs-airplay PROPERTIES
    PREFIX ""
    OUTPUT_NAME obs-airplay
    LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/obs-plugins/64bit
    RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/obs-plugins/64bit
)

# Install
install(TARGETS obs-airplay
    LIBRARY DESTINATION obs-plugins/64bit
    RUNTIME DESTINATION obs-plugins/64bit
)

install(FILES data/locale/en-US.ini
    DESTINATION data/obs-plugins/obs-airplay/locale/en-US
)
