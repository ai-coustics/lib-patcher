#include <stdio.h>
#include <stdint.h>

// External functions from the patched mylib
extern int32_t mylib_add(int32_t a, int32_t b);
extern int32_t mylib_multiply(int32_t a, int32_t b);
extern int32_t mylib_get_magic_number(void);

int main(void) {
    printf("Testing patched mylib from C\n");
    printf("================================\n\n");

    int32_t result_add = mylib_add(5, 3);
    printf("mylib_add(5, 3) = %d\n", result_add);
    if (result_add != 8) {
        fprintf(stderr, "ERROR: Expected 8, got %d\n", result_add);
        return 1;
    }

    int32_t result_multiply = mylib_multiply(4, 7);
    printf("mylib_multiply(4, 7) = %d\n", result_multiply);
    if (result_multiply != 28) {
        fprintf(stderr, "ERROR: Expected 28, got %d\n", result_multiply);
        return 1;
    }

    int32_t result_magic = mylib_get_magic_number();
    printf("mylib_get_magic_number() = %d\n", result_magic);
    if (result_magic != 42) {
        fprintf(stderr, "ERROR: Expected 42, got %d\n", result_magic);
        return 1;
    }

    printf("\n✓ All tests passed!\n");
    printf("✓ Successfully called patched Rust library from C\n");
    
    return 0;
}
