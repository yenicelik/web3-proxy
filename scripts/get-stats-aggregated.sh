# Make a get request to get the stats in an aggregated fashion

# I dont think we need a user id ...


curl -X GET \
"http://localhost:8544/user/stats/aggregate?query_start=1678463411&query_window_seconds=1000000"
