# Keep wire-protocol constants used via reflection-free paths
-keepattributes *Annotation*, InnerClasses
-dontnote kotlinx.serialization.**
# DataStore / coroutines safe defaults
-keepclassmembers class kotlinx.coroutines.** { volatile <fields>; }
