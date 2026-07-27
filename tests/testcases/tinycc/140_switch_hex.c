/* Test switch statements with hex constants in the 0xc0-0xcf range.
   This mirrors the switch in TCC's tok_str_add2. */
#include <stdio.h>

static int classify(int t) {
    switch (t) {
    case 0xc2: return 1;
    case 0xc3: return 2;
    case 0xc0: return 3;
    case 0xc1: return 4;
    case 0xca: return 5;
    case 0xcf: return 6;
    case 0xcd: return 7;
    case 0xce: return 8;
    case 0xc8: return 9;
    case 0xc9: return 10;
    case 0xcb: return 11;
    case 0xc4: return 12;
    case 0xc5: return 13;
    case 0xc6: return 14;
    case 0xc7: return 15;
    case 0xcc: return 16;
    default:   return 0;
    }
}

int main(void) {
    int i;
    for (i = 0xbf; i <= 0xd1; i++) {
        printf("0x%02x -> %d\n", i, classify(i));
    }
    return 0;
}
