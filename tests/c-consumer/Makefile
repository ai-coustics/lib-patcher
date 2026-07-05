# Makefile for testing patched Rust library from C
# This is for Linux/macOS only. Windows uses cl.exe in the GitHub workflow.

CC = gcc
CFLAGS = -Wall -Wextra -std=c11
LDFLAGS = -L../testlib/target/release -lpthread -ldl -lm

# Determine platform
UNAME_S := $(shell uname -s)

LIB_FILE = ../testlib/target/release/libtestlib_patched.a

# macOS needs the Security framework; Linux needs nothing extra.
ifeq ($(UNAME_S),Darwin)
    LDFLAGS += -framework Security
endif

all: testlib-test

testlib-test: main.c $(LIB_FILE)
	$(CC) $(CFLAGS) -o testlib-test main.c $(LIB_FILE) $(LDFLAGS)

clean:
	rm -f testlib-test

.PHONY: all clean
