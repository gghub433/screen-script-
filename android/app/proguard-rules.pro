# JNI: method names and the callback interface are looked up by name from Rust.
-keep class app.revizor.core.Native { *; }
-keep class app.revizor.core.Native$Callback { *; }
-keepclassmembers class * implements app.revizor.core.Native$Callback { public void onEvent(int, long[], java.lang.String); }
