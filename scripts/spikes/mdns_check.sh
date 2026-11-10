#!/bin/bash
# Spike D (desktop side): advertise + browse a _pinchshare._tcp service with the OS mDNS responder.
dns-sd -R "PinchShare-spike" _pinchshare._tcp local 47800 id=spike >/dev/null 2>&1 & R=$!
sleep 2
( dns-sd -B _pinchshare._tcp local & B=$!; sleep 3; kill $B ) 2>&1 | grep -E "Add|PinchShare" 
dns-sd -L "PinchShare-spike" _pinchshare._tcp local & L=$!; sleep 3; kill $L 2>/dev/null
kill $R 2>/dev/null
