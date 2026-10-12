# Builds only the ESP-IDF second-stage bootloader with app rollback enabled.
cmake_minimum_required(VERSION 3.16)
include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(esp_lan_ota_bootloader)
