# Makefile for testing patched Rust library from C
# This is for Linux/macOS only. Windows uses cl.exe in the GitHub workflow.

CC = gcc
CFLAGS = -Wall -Wextra -std=c11
LDFLAGS = -L../testlib/target/release -lpthread -ldl -lm

# Determine platform
UNAME_S := $(shell uname -s)

ifeq ($(UNAME_S),Darwin)
    # macOS
    LDFLAGS += -framework Security
    LIB_FILE = ../testlib/target/release/libtestlib_patched.a
else
    # Linux
    LDFLAGS +=
    LIB_FILE = ../testlib/target/release/libtestlib_patched.a
endif

all: testlib-test

testlib-test: main.c $(LIB_FILE)
	$(CC) $(CFLAGS) -o testlib-test main.c $(LIB_FILE) $(LDFLAGS)

clean:
	rm -f testlib-test

.PHONY: all clean
