//go:build datad_embedded

package main

/*
#include <stdlib.h>
#include <stdio.h>
#include "datad_entry.h"
*/
import "C"

import (
	"os"
	"unsafe"
)

func main() {
	args := make([]*C.char, len(os.Args)+1)
	for i, arg := range os.Args {
		args[i] = C.CString(arg)
	}
	code := C.datad_main(C.int(len(os.Args)), (**C.char)(unsafe.Pointer(&args[0])))
	C.fflush(nil)
	for _, arg := range args[:len(os.Args)] {
		C.free(unsafe.Pointer(arg))
	}
	os.Exit(int(code))
}
