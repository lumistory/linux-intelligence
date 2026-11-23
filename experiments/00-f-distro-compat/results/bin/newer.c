#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
int main(void) {
  char b[16];
  memset(b, 0, sizeof b);
  memset_explicit(b, 'x', 4);   /* glibc >= 2.43 */
  printf("hello-newer: %s\n", b);
  return 0;
}
