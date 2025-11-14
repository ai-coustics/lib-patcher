#include <stdio.h>
#include <stdint.h>

// External functions from the patched testlib
extern int32_t testlib_add(int32_t a, int32_t b);
extern int32_t testlib_multiply(int32_t a, int32_t b);
extern int32_t testlib_random_number(int32_t max);
extern int32_t testlib_process_json(void);
extern int32_t testlib_use_hashmap(void);
extern int32_t testlib_format_string(void);
extern int32_t testlib_vec_operations(void);
extern int32_t testlib_get_magic(void);

int main(void) {
    int errors = 0;

    printf("Testing patched testlib from C\n");
    printf("=================================\n\n");

    // Test basic arithmetic
    int32_t result_add = testlib_add(10, 5);
    printf("testlib_add(10, 5) = %d ", result_add);
    if (result_add == 15) {
        printf("✓\n");
    } else {
        printf("✗ (expected 15)\n");
        errors++;
    }

    int32_t result_multiply = testlib_multiply(6, 7);
    printf("testlib_multiply(6, 7) = %d ", result_multiply);
    if (result_multiply == 42) {
        printf("✓\n");
    } else {
        printf("✗ (expected 42)\n");
        errors++;
    }

    // Test random number (just check it's in range)
    int32_t random = testlib_random_number(100);
    printf("testlib_random_number(100) = %d ", random);
    if (random >= 0 && random < 100) {
        printf("✓\n");
    } else {
        printf("✗ (expected 0-99)\n");
        errors++;
    }

    // Test JSON processing
    int32_t json_result = testlib_process_json();
    printf("testlib_process_json() = %d ", json_result);
    if (json_result == 1) {
        printf("✓\n");
    } else {
        printf("✗ (expected 1)\n");
        errors++;
    }

    // Test HashMap
    int32_t hashmap_result = testlib_use_hashmap();
    printf("testlib_use_hashmap() = %d ", hashmap_result);
    if (hashmap_result == 60) {
        printf("✓\n");
    } else {
        printf("✗ (expected 60)\n");
        errors++;
    }

    // Test string formatting
    int32_t format_result = testlib_format_string();
    printf("testlib_format_string() = %d ", format_result);
    if (format_result == 1) {
        printf("✓\n");
    } else {
        printf("✗ (expected 1)\n");
        errors++;
    }

    // Test Vec operations
    int32_t vec_result = testlib_vec_operations();
    printf("testlib_vec_operations() = %d ", vec_result);
    if (vec_result == 2450) {
        printf("✓\n");
    } else {
        printf("✗ (expected 2450)\n");
        errors++;
    }

    // Test magic number
    int32_t magic = testlib_get_magic();
    printf("testlib_get_magic() = %d ", magic);
    if (magic == 123) {
        printf("✓\n");
    } else {
        printf("✗ (expected 123)\n");
        errors++;
    }

    printf("\n=================================\n");
    if (errors == 0) {
        printf("✓ All tests passed!\n");
        printf("✓ Successfully called patched Rust library from C\n");
        return 0;
    } else {
        printf("✗ %d test(s) failed!\n", errors);
        return 1;
    }
}
