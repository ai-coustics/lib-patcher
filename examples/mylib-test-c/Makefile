# Detect OS
UNAME_S := $(shell uname -s)

# Set variables based on OS
ifeq ($(UNAME_S),Linux)
    CC = gcc
    PATCHED_LIB = ../mylib/target/release/libmylib_patched.a
    LIBS = -lpthread -ldl -lm
    OUTPUT = mylib-test-c
endif
ifeq ($(UNAME_S),Darwin)
    CC = clang
    PATCHED_LIB = ../mylib/target/release/libmylib_patched.a
    LIBS = -framework Security
    OUTPUT = mylib-test-c
endif
ifeq ($(OS),Windows_NT)
    CC = cl
    PATCHED_LIB = ..\mylib\target\release\mylib_patched.lib
    LIBS = ws2_32.lib advapi32.lib userenv.lib
    OUTPUT = mylib-test-c.exe
endif

CFLAGS = -Wall -Wextra -std=c11

.PHONY: all clean

all: $(OUTPUT)

$(OUTPUT): main.c $(PATCHED_LIB)
	@echo "Building c lib..."
ifeq ($(OS),Windows_NT)
	$(CC) /Fe:$(OUTPUT) main.c $(PATCHED_LIB) $(LIBS)
else
	$(CC) $(CFLAGS) -o $(OUTPUT) main.c $(PATCHED_LIB) $(LIBS)
endif
	@echo "✓ Build complete"

clean:
	rm -f $(OUTPUT) mylib-test-c *.o *.obj

run: $(OUTPUT)
	@echo "Running C consumer..."
	@./$(OUTPUT)

.DEFAULT_GOAL := all
